// SPDX-License-Identifier: Apache-2.0
//! Core operation log logic — packed single-file format.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use objects::{
    error::{HeddleError, Result},
    lock::{RepoLock, WriteLockGuard},
    object::Principal,
};

use super::{
    oplog_backend::OpLogBackend,
    oplog_types::{
        ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
        isolation_keys_for_record,
    },
    packed_oplog::{PackedOpLog, PackedOpLogIndex},
};

/// A `TransactionCommit` marker carries no user-facing operation: it is the
/// atomic commit sentinel, not a forward op. The undo/redo eligibility scans
/// ignore it so a record-less transaction (e.g. an `undo`/`redo` whose commit
/// batch holds only the marker) is never itself selected as an undoable or
/// redoable unit. A batch with at least one *non-marker* entry (the common
/// `[op, …, TransactionCommit]` shape) still qualifies on that entry.
fn is_transaction_commit(op: &OpRecord) -> bool {
    matches!(op, OpRecord::TransactionCommit { .. })
}

/// Operation log for tracking operations and enabling undo.
pub struct OpLog {
    pub(crate) root: PathBuf,
    cached: Mutex<Option<PackedOpLogIndex>>,
    actor: Arc<Principal>,
}

impl OpLog {
    pub fn new(heddle_dir: impl AsRef<Path>, actor: Principal) -> Self {
        Self {
            root: heddle_dir.as_ref().to_path_buf(),
            cached: Mutex::new(None),
            actor: Arc::new(actor),
        }
    }

    /// Convenience constructor for tests and one-shot tooling that
    /// don't have a real principal in context. Stamps every recorded
    /// entry with `("<unknown>", "")`. Production code in `Repository`
    /// uses [`OpLog::new`] with the configured principal.
    pub fn new_unattributed(heddle_dir: impl AsRef<Path>) -> Self {
        Self::new(heddle_dir, Principal::new("<unknown>", ""))
    }

    /// Initialize the oplog directory and create an empty oplog.bin.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(self.oplog_dir())?;
        std::fs::create_dir_all(self.root.join("locks"))?;
        let path = self.oplog_path();
        if !path.exists() {
            let log = PackedOpLog::new(path);
            log.save()?;
        }
        Ok(())
    }

    fn oplog_dir(&self) -> PathBuf {
        self.root.join("oplog")
    }

    fn oplog_path(&self) -> PathBuf {
        self.oplog_dir().join("oplog.bin")
    }

    fn write_lock(&self) -> Result<WriteLockGuard> {
        let lock_path = self.root.join("locks/oplog.lock");
        RepoLock::at(lock_path)
            .write()
            .map_err(|err| HeddleError::Config(format!("failed to acquire oplog lock: {err}")))
    }

    fn build_entries(
        actor: &Arc<Principal>,
        operations: Vec<OpRecord>,
        start_id: u64,
        timestamp: chrono::DateTime<Utc>,
        scope: &Option<String>,
    ) -> Vec<OpEntry> {
        operations
            .into_iter()
            .enumerate()
            .map(|(index, operation)| OpEntry {
                id: start_id + index as u64,
                timestamp,
                operation,
                undone: false,
                batch_id: start_id,
                batch_index: index as u32,
                scope: scope.clone(),
                actor: Arc::clone(actor),
                operation_id: None,
            })
            .collect()
    }

    fn ensure_current_format(&self) -> Result<()> {
        let path = self.oplog_path();
        if !path.exists() {
            return Ok(());
        }
        if PackedOpLog::is_latest(&path)? {
            let _ = PackedOpLogIndex::open(&path)?;
            return Ok(());
        }
        let _lock = self.write_lock()?;
        PackedOpLog::ensure_latest(&path)
    }

    /// Load from disk, bypassing cache (used after acquiring write lock).
    fn load_fresh_for_write(&self) -> Result<PackedOpLog> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::ensure_latest(&path)?;
            PackedOpLog::load(&path)
        } else {
            Ok(PackedOpLog::new(path))
        }
    }

    fn open_index_for_write(&self) -> Result<PackedOpLogIndex> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::ensure_latest(&path)?;
        } else {
            PackedOpLog::new(path.clone()).save()?;
        }
        PackedOpLogIndex::open(&path)
    }

    /// Load from cache or disk (for read operations).
    fn load_cached(&self) -> Result<std::sync::MutexGuard<'_, Option<PackedOpLogIndex>>> {
        let guard = self.cached.lock().unwrap();
        if guard.is_some() {
            return Ok(guard);
        }
        drop(guard);

        self.ensure_current_format()?;
        let mut guard = self.cached.lock().unwrap();
        if guard.is_none() {
            let path = self.oplog_path();
            *guard = Some(if path.exists() {
                PackedOpLogIndex::open(&path)?
            } else {
                PackedOpLogIndex::empty(path)
            });
        }
        Ok(guard)
    }

    /// Force a fresh disk load into the cache, returning the refreshed guard.
    /// Read paths that must observe a CROSS-PROCESS commit use this instead of
    /// [`load_cached`]: a long-lived handle's already-populated cache is a stale
    /// view that would miss a batch another process wrote (heddle#354 r6, cid
    /// 3329711888).
    fn refresh_cached(&self) -> Result<std::sync::MutexGuard<'_, Option<PackedOpLogIndex>>> {
        self.ensure_current_format()?;
        let mut guard = self.cached.lock().unwrap();
        let path = self.oplog_path();
        *guard = Some(if path.exists() {
            PackedOpLogIndex::open(&path)?
        } else {
            PackedOpLogIndex::empty(path)
        });
        Ok(guard)
    }

    /// Force this handle's in-memory cache to reload from disk, so it observes
    /// commits written through a DIFFERENT `OpLog` handle of the same file —
    /// e.g. the refs write chokepoint's committer, which appends via its own
    /// fresh handle (the `refs`→`repo` seam). Without this, a long-lived handle
    /// (the mount/daemon's `repo.oplog()`) keeps a stale cache after a
    /// `commit_and_publish` and a same-process `recent()` would miss the just
    /// committed batch (heddle#354 r8). Same staleness class as the
    /// cross-process case the reconciler already refreshes for (cid 3329711888).
    pub fn refresh_cache(&self) -> Result<()> {
        let _guard = self.refresh_cached()?;
        Ok(())
    }

    /// Get the last operation entry.
    pub fn last(&self) -> Result<Option<OpEntry>> {
        let guard = self.load_cached()?;
        guard.as_ref().unwrap().last_entry()
    }

    /// Get the last N operations.
    pub fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        let guard = self.load_cached()?;
        guard.as_ref().unwrap().recent_entries(count)
    }

    pub fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped(count, None)
    }

    pub fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(count, |_| true, scope)
    }

    pub fn recent_batches_after_scoped(
        &self,
        since_head_id: u64,
        count: usize,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        let guard = self.refresh_cached()?;
        guard
            .as_ref()
            .unwrap()
            .collect_batches_after_scoped(since_head_id, count, |_| true, scope)
    }

    /// Like [`recent_batches_scoped`](Self::recent_batches_scoped) but counts
    /// only **user-facing** batches: the record-less `TransactionCommit`
    /// sentinels an `undo`/`redo` appends are dropped by the predicate BEFORE
    /// the `count` limit applies, so `--depth N` yields N real operations even
    /// when the newest batch is a commit marker. Filtering *after* a fixed-count
    /// fetch returned empty for `--depth 1` whenever the latest op was itself an
    /// undo/redo (heddle#355 cid 3330867777).
    pub fn recent_user_batches_scoped(
        &self,
        count: usize,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(count, |batch| !batch.is_transaction_marker_only(), scope)
    }

    pub fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped(count, None)
    }

    pub fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(
            count,
            |batch| {
                batch
                    .entries
                    .iter()
                    .any(|e| !e.undone && !is_transaction_commit(&e.operation))
            },
            scope,
        )
    }

    pub fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped(count, None)
    }

    pub fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(
            count,
            |batch| {
                batch
                    .entries
                    .iter()
                    .any(|e| e.undone && !is_transaction_commit(&e.operation))
            },
            scope,
        )
    }

    /// Mark a batch as undone.
    pub fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
        self.update_batch_undone_state(batch, true)
    }

    /// Mark a batch as redone.
    pub fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        self.update_batch_undone_state(batch, false)
    }

    /// Coalesce two existing batches into one logical undo/redo unit.
    ///
    /// This is intentionally narrow: it rewrites only batch metadata for
    /// already-recorded entries. Forward side effects must already be durable
    /// before callers use this to present them as one operation.
    pub fn coalesce_batches(
        &self,
        primary_batch_id: u64,
        secondary_batch_id: u64,
    ) -> Result<OpBatch> {
        if primary_batch_id == secondary_batch_id {
            let mut batches =
                self.collect_batches_scoped(1, |batch| batch.id == primary_batch_id, None)?;
            return batches.pop().ok_or_else(|| {
                HeddleError::Config(format!("oplog batch {primary_batch_id} not found"))
            });
        }

        let _lock = self.write_lock()?;
        let mut packed = self.load_fresh_for_write()?;
        let mut matching_indices = Vec::new();
        let mut saw_primary = false;
        let mut saw_secondary = false;

        for (idx, entry) in packed.entries.iter().enumerate() {
            let batch_id = if entry.batch_id == 0 {
                entry.id
            } else {
                entry.batch_id
            };
            if batch_id == primary_batch_id {
                saw_primary = true;
                matching_indices.push(idx);
            } else if batch_id == secondary_batch_id {
                saw_secondary = true;
                matching_indices.push(idx);
            }
        }

        if !saw_primary || !saw_secondary {
            return Err(HeddleError::Config(format!(
                "cannot coalesce missing oplog batch(es): primary={primary_batch_id}, secondary={secondary_batch_id}"
            )));
        }

        matching_indices.sort_by_key(|idx| packed.entries[*idx].id);
        for (batch_index, entry_idx) in matching_indices.iter().copied().enumerate() {
            let entry = &mut packed.entries[entry_idx];
            entry.batch_id = primary_batch_id;
            entry.batch_index = batch_index as u32;
        }

        packed.save()?;
        let entries = matching_indices
            .into_iter()
            .map(|idx| packed.entries[idx].clone())
            .collect::<Vec<_>>();
        *self.cached.lock().unwrap() = Some(PackedOpLogIndex::open(&self.oplog_path())?);

        Ok(OpBatch {
            id: primary_batch_id,
            entries,
        })
    }

    /// Record a batch of operations.
    pub fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }

    pub fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        if operations.is_empty() {
            return Ok(Vec::new());
        }

        let _lock = self.write_lock()?;
        // Reload from disk to catch any writes from other processes
        let index = self.open_index_for_write()?;

        let start_id = index.head_id() + 1;
        let timestamp = Utc::now();
        let scope_owned = scope.map(str::to_string);
        let new_entries =
            Self::build_entries(&self.actor, operations, start_id, timestamp, &scope_owned);
        let ids: Vec<u64> = new_entries.iter().map(|e| e.id).collect();

        let updated = index.append_entries(&new_entries)?;
        *self.cached.lock().unwrap() = Some(updated);

        Ok(ids)
    }

    /// Atomic dedup+append for transaction-scoped batches.
    ///
    /// Scans the most recent `recent_window` batches for an
    /// `OpRecord::TransactionCommit { transaction_id: id, .. }` marker
    /// matching `transaction_id`. If one is present the batch was
    /// already committed (e.g. by a prior crash-recovery retry) and
    /// this call returns `Ok(None)` without writing. Otherwise the
    /// batch is appended and `Ok(Some(ids))` is returned.
    ///
    /// Pre-r4 the rebase helper did the existence check and the append
    /// in two separate oplog calls with no shared lock, so two
    /// concurrent `rebase --continue` invocations sharing one
    /// persisted `transaction_id` could both observe "not committed"
    /// and both append — reintroducing the duplicate-batch hazard from
    /// r2 (heddle#198 r4 / Codex PR #218 P2). This method holds the
    /// existing oplog write lock across both the scan and the append,
    /// so the two operations are atomic with respect to any other
    /// oplog writer in the process or on the host.
    pub fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        if operations.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let _lock = self.write_lock()?;
        let index = self.open_index_for_write()?;

        let recent = index.collect_batches_scoped(recent_window, |_| true, scope)?;
        if recent.iter().any(|batch| {
            batch.entries.iter().any(|entry| {
                matches!(
                    &entry.operation,
                    OpRecord::TransactionCommit { transaction_id: id, .. }
                        if id == transaction_id
                )
            })
        }) {
            return Ok(None);
        }

        let start_id = index.head_id() + 1;
        let timestamp = Utc::now();
        let scope_owned = scope.map(str::to_string);
        let new_entries =
            Self::build_entries(&self.actor, operations, start_id, timestamp, &scope_owned);
        let ids: Vec<u64> = new_entries.iter().map(|e| e.id).collect();

        let updated = index.append_entries(&new_entries)?;
        *self.cached.lock().unwrap() = Some(updated);

        Ok(Some(ids))
    }

    /// Append a transaction batch **exactly once**, deduplicated by an
    /// **unbounded** scan over the entire committed history for a
    /// matching `OpRecord::TransactionCommit { transaction_id }` marker —
    /// the linearization point for the atomic-mutation primitive
    /// (heddle#330 §2.2).
    ///
    /// Unlike [`OpLog::record_batch_scoped_if_no_transaction`], which scans
    /// only a caller-supplied recent window and concedes that "ageing past
    /// it duplicates the batch", this is exact-once at **any** retry timing
    /// — including a delayed crash-retry after an arbitrary number of
    /// intervening commits — because the lookup domain is the whole log.
    /// The scan and the append are held under the same oplog write lock, so
    /// two concurrent retriers serialize and exactly one wins.
    ///
    /// `operations` must already carry the `TransactionCommit` marker whose
    /// `transaction_id` matches the `transaction_id` argument. Returns
    /// `Ok(None)` when the transaction was already committed by a prior
    /// (possibly long-ago) run.
    pub fn record_batch_exactly_once(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
    ) -> Result<Option<Vec<u64>>> {
        if operations.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let _lock = self.write_lock()?;
        let index = self.open_index_for_write()?;

        if index.transaction_commit(transaction_id)?.is_some() {
            return Ok(None);
        }

        let start_id = index.head_id() + 1;
        let timestamp = Utc::now();
        let scope_owned = scope.map(str::to_string);
        let new_entries =
            Self::build_entries(&self.actor, operations, start_id, timestamp, &scope_owned);
        let ids: Vec<u64> = new_entries.iter().map(|e| e.id).collect();

        let updated = index.append_entries(&new_entries)?;
        *self.cached.lock().unwrap() = Some(updated);

        Ok(Some(ids))
    }

    pub fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        if operations.is_empty() {
            return Ok(ConditionalCommitOutcome::Committed(Vec::new()));
        }

        let _lock = self.write_lock()?;
        let index = self.open_index_for_write()?;

        if index.transaction_commit(transaction_id)?.is_some() {
            let committed = index.committed_batch_records(transaction_id)?;
            return Ok(ConditionalCommitOutcome::AlreadyCommitted(committed));
        }

        if !precondition.keys.is_empty() && index.head_id() != precondition.since_head_id {
            for entry in index.entries_after(precondition.since_head_id)? {
                let touched = isolation_keys_for_record(&entry.operation, entry.scope.as_deref());
                if let Some(key) = touched.intersection(&precondition.keys).next().cloned() {
                    return Ok(ConditionalCommitOutcome::IsolationConflict {
                        key,
                        since_head_id: precondition.since_head_id,
                        conflicting_entry_id: entry.id,
                    });
                }
            }
        }

        let start_id = index.head_id() + 1;
        let timestamp = Utc::now();
        let scope_owned = scope.map(str::to_string);
        let new_entries =
            Self::build_entries(&self.actor, operations, start_id, timestamp, &scope_owned);
        let ids: Vec<u64> = new_entries.iter().map(|e| e.id).collect();

        let updated = index.append_entries(&new_entries)?;
        *self.cached.lock().unwrap() = Some(updated);

        Ok(ConditionalCommitOutcome::Committed(ids))
    }

    /// Current oplog generation — the monotonic `head_id`
    /// (`packed_oplog.rs` leading field). Reads only the **fixed-size header**
    /// from disk (not the whole log), so it is the cheap O(1) gate the per-read
    /// reconciliation needs (heddle#330 §2.2): a long-held handle observes a
    /// concurrent commit's advance, and the no-lag hot path is one header read +
    /// one integer compare against a cached watermark — no tail scan. Returns 0
    /// when the oplog file does not exist yet.
    pub fn head_id(&self) -> Result<u64> {
        self.ensure_current_format()?;
        match PackedOpLog::read_head_id(&self.oplog_path()) {
            Ok(id) => Ok(id),
            // Treat a not-yet-created oplog as generation 0.
            Err(_) if !self.oplog_path().exists() => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Header-only v3 `head_id` read for I/O benchmarks.
    ///
    /// Production callers should use [`OpLog::head_id`], which also handles
    /// current-format validation and migration. The benchmark fixtures are
    /// already seeded as v3, so this exposes the raw fixed-header read without
    /// widening the packed-oplog internals.
    #[cfg(feature = "bench")]
    pub fn read_head_id_for_bench(&self) -> Result<u64> {
        PackedOpLog::read_head_id(&self.oplog_path())
    }

    /// Replace the on-disk oplog with prebuilt entries for I/O benchmarks.
    ///
    /// This keeps benchmark setup deterministic and cheap without widening the
    /// packed-oplog model itself. Production code must append through the
    /// normal record paths so locking, attribution, and transaction semantics
    /// stay centralized.
    #[cfg(feature = "bench")]
    pub fn write_entries_for_bench(&self, entries: Vec<OpEntry>) -> Result<()> {
        std::fs::create_dir_all(self.oplog_dir())?;
        std::fs::create_dir_all(self.root.join("locks"))?;
        let head_id = entries.last().map(|entry| entry.id).unwrap_or(0);
        let mut packed = PackedOpLog::new(self.oplog_path());
        packed.entries = entries;
        packed.head_id = head_id;
        packed.save()?;
        *self.cached.lock().unwrap() = Some(PackedOpLogIndex::open(&self.oplog_path())?);
        Ok(())
    }

    /// The non-marker records of the batch that committed `transaction_id`
    /// (heddle#354 r5, cid 3329631075) — i.e. the batch whose `TransactionCommit`
    /// marker carries that id, minus the marker itself. A crash-retry that
    /// dedup-hits an already-committed transaction uses these to reconstruct the
    /// ORIGINAL committed identity, instead of returning this run's freshly
    /// (re-)generated value, which may diverge from what was actually persisted.
    ///
    /// Unbounded scan, matching the dedup domain of
    /// [`record_batch_exactly_once`](Self::record_batch_exactly_once). Returns an
    /// empty vec if no batch committed that id, or if the batch held only its
    /// marker.
    pub fn committed_batch_records(&self, transaction_id: &str) -> Result<Vec<OpRecord>> {
        // Refresh, don't trust the cache: this is only ever reached on a dedup
        // hit, which may be CROSS-PROCESS — another process committed the batch
        // while this long-lived handle's cache stayed stale. A cached read would
        // fail to find the batch and reconstruct a miss (heddle#354 r6, cid
        // 3329711888). The committed batch is durable and append-only, so a
        // lock-free fresh read observes it consistently.
        let guard = self.refresh_cached()?;
        guard
            .as_ref()
            .unwrap()
            .committed_batch_records(transaction_id)
    }

    pub(super) fn record_single_scoped(
        &self,
        operation: OpRecord,
        scope: Option<&str>,
    ) -> Result<u64> {
        let _lock = self.write_lock()?;
        let index = self.open_index_for_write()?;

        let id = index.head_id() + 1;
        let entry = OpEntry {
            id,
            timestamp: Utc::now(),
            operation,
            undone: false,
            batch_id: id,
            batch_index: 0,
            scope: scope.map(str::to_string),
            actor: self.actor.clone(),
            operation_id: None,
        };

        let updated = index.append_entries(&[entry])?;
        *self.cached.lock().unwrap() = Some(updated);

        Ok(id)
    }

    pub(super) fn record_single(&self, operation: OpRecord) -> Result<u64> {
        self.record_single_scoped(operation, None)
    }

    fn update_batch_undone_state(&self, batch: &OpBatch, undone: bool) -> Result<OpBatch> {
        let _lock = self.write_lock()?;
        let mut packed = self.load_fresh_for_write()?;
        packed.set_undone(batch.id, undone);
        packed.save()?;

        let mut updated_entries = batch.entries.clone();
        for e in &mut updated_entries {
            e.undone = undone;
        }

        *self.cached.lock().unwrap() = Some(PackedOpLogIndex::open(&self.oplog_path())?);

        Ok(OpBatch {
            id: batch.id,
            entries: updated_entries,
        })
    }

    fn collect_batches_scoped<F>(
        &self,
        count: usize,
        predicate: F,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>>
    where
        F: Fn(&OpBatch) -> bool,
    {
        let guard = self.load_cached()?;
        guard
            .as_ref()
            .unwrap()
            .collect_batches_scoped(count, predicate, scope)
    }
}

impl OpLogBackend for OpLog {
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        OpLog::record_batch_scoped(self, operations, scope)
    }

    async fn record_batch_scoped_if_no_transaction(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        recent_window: usize,
    ) -> Result<Option<Vec<u64>>> {
        OpLog::record_batch_scoped_if_no_transaction(
            self,
            operations,
            scope,
            transaction_id,
            recent_window,
        )
    }

    fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        OpLog::record_batch_exactly_once_if_unchanged(
            self,
            operations,
            scope,
            transaction_id,
            precondition,
        )
    }

    fn last(&self) -> Result<Option<OpEntry>> {
        OpLog::last(self)
    }

    fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        OpLog::recent(self, count)
    }

    async fn recent_batches_scoped(
        &self,
        count: usize,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        OpLog::recent_batches_scoped(self, count, scope)
    }

    async fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        OpLog::undo_batches_scoped(self, count, scope)
    }

    async fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        OpLog::redo_batches_scoped(self, count, scope)
    }

    fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
        OpLog::mark_batch_undone(self, batch)
    }

    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        OpLog::mark_batch_redone(self, batch)
    }

    fn coalesce_batches(&self, primary_batch_id: u64, secondary_batch_id: u64) -> Result<OpBatch> {
        OpLog::coalesce_batches(self, primary_batch_id, secondary_batch_id)
    }
}
