// SPDX-License-Identifier: Apache-2.0
//! Core operation log logic — packed single-file format.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use chrono::Utc;
use objects::{
    error::{HeddleError, Result},
    lock::{RepoLock, WriteLockGuard},
    object::Principal,
};

use super::{
    oplog_backend::OpLogBackend,
    oplog_types::{OpBatch, OpEntry, OpRecord},
    packed_oplog::PackedOpLog,
};

/// Operation log for tracking operations and enabling undo.
pub struct OpLog {
    pub(crate) root: PathBuf,
    cached: Mutex<Option<PackedOpLog>>,
    /// Principal stamped on every newly recorded `OpEntry`. Set at
    /// construction from `RepoConfig.principal`; the open path in
    /// `Repository::open_raw` always supplies one. Per-operation
    /// principal threading (e.g. distinct agents within one repo) is a
    /// future refactor — see the `--op-id` wiring follow-up — at which
    /// point this field becomes the default and individual record_*
    /// calls override it.
    actor: Principal,
}

impl OpLog {
    /// Create an oplog whose entries are recorded under `actor`. The
    /// `Repository` open path always passes the configured principal;
    /// when the repo has no principal configured, callers should pass
    /// [`Principal::new("<unknown>", "")`] explicitly rather than
    /// reaching for a sentinel.
    pub fn new(heddle_dir: impl AsRef<Path>, actor: Principal) -> Self {
        Self {
            root: heddle_dir.as_ref().to_path_buf(),
            cached: Mutex::new(None),
            actor,
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

    /// Load from disk, bypassing cache (used after acquiring write lock).
    fn load_fresh(&self) -> Result<PackedOpLog> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::load(&path)
        } else {
            Ok(PackedOpLog::new(path))
        }
    }

    /// Load from cache or disk (for read operations).
    fn load_cached(&self) -> Result<std::sync::MutexGuard<'_, Option<PackedOpLog>>> {
        let mut guard = self.cached.lock().unwrap();
        if guard.is_none() {
            *guard = Some(self.load_fresh()?);
        }
        Ok(guard)
    }

    /// Get the last operation entry.
    pub fn last(&self) -> Result<Option<OpEntry>> {
        let guard = self.load_cached()?;
        Ok(guard.as_ref().unwrap().last_entry().cloned())
    }

    /// Get the last N operations.
    pub fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        let guard = self.load_cached()?;
        Ok(guard.as_ref().unwrap().recent_entries(count))
    }

    /// Get the most recent N batches.
    pub fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.collect_batches(count, |_| true)
    }

    pub fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(count, |_| true, scope)
    }

    /// Get the next undoable batches (most recent first).
    pub fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.collect_batches(count, |batch| batch.entries.iter().any(|e| !e.undone))
    }

    pub fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(
            count,
            |batch| batch.entries.iter().any(|e| !e.undone),
            scope,
        )
    }

    /// Get the next redoable batches (most recent first).
    pub fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.collect_batches(count, |batch| batch.entries.iter().any(|e| e.undone))
    }

    pub fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.collect_batches_scoped(count, |batch| batch.entries.iter().any(|e| e.undone), scope)
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
        let mut packed = self.load_fresh()?;
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
        *self.cached.lock().unwrap() = Some(packed);

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
        let mut packed = self.load_fresh()?;

        let start_id = packed.head_id + 1;
        let timestamp = Utc::now();
        let mut new_entries = Vec::with_capacity(operations.len());
        let mut ids = Vec::with_capacity(operations.len());

        for (index, operation) in operations.into_iter().enumerate() {
            let id = start_id + index as u64;
            new_entries.push(OpEntry {
                id,
                timestamp,
                operation,
                undone: false,
                batch_id: start_id,
                batch_index: index as u32,
                scope: scope.map(str::to_string),
                actor: self.actor.clone(),
                operation_id: None,
            });
            ids.push(id);
        }

        packed.append(new_entries);
        packed.save()?;

        // Update in-process cache
        *self.cached.lock().unwrap() = Some(packed);

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
        let mut packed = self.load_fresh()?;

        let recent =
            packed.collect_batches_scoped(recent_window, |_| true, scope);
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

        let start_id = packed.head_id + 1;
        let timestamp = Utc::now();
        let mut new_entries = Vec::with_capacity(operations.len());
        let mut ids = Vec::with_capacity(operations.len());
        for (index, operation) in operations.into_iter().enumerate() {
            let id = start_id + index as u64;
            new_entries.push(OpEntry {
                id,
                timestamp,
                operation,
                undone: false,
                batch_id: start_id,
                batch_index: index as u32,
                scope: scope.map(str::to_string),
                actor: self.actor.clone(),
                operation_id: None,
            });
            ids.push(id);
        }

        packed.append(new_entries);
        packed.save()?;
        *self.cached.lock().unwrap() = Some(packed);

        Ok(Some(ids))
    }

    pub(super) fn record_single_scoped(
        &self,
        operation: OpRecord,
        scope: Option<&str>,
    ) -> Result<u64> {
        let _lock = self.write_lock()?;
        let mut packed = self.load_fresh()?;

        let id = packed.head_id + 1;
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

        packed.append(vec![entry]);
        packed.save()?;
        *self.cached.lock().unwrap() = Some(packed);

        Ok(id)
    }

    pub(super) fn record_single(&self, operation: OpRecord) -> Result<u64> {
        self.record_single_scoped(operation, None)
    }

    fn update_batch_undone_state(&self, batch: &OpBatch, undone: bool) -> Result<OpBatch> {
        let _lock = self.write_lock()?;
        let mut packed = self.load_fresh()?;
        packed.set_undone(batch.id, undone);
        packed.save()?;

        // Return updated batch
        let updated_entries: Vec<OpEntry> = batch
            .entries
            .iter()
            .map(|e| OpEntry {
                undone,
                ..e.clone()
            })
            .collect();

        *self.cached.lock().unwrap() = Some(packed);

        Ok(OpBatch {
            id: batch.id,
            entries: updated_entries,
        })
    }

    fn collect_batches<F>(&self, count: usize, predicate: F) -> Result<Vec<OpBatch>>
    where
        F: Fn(&OpBatch) -> bool,
    {
        self.collect_batches_scoped(count, predicate, None)
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
        Ok(guard
            .as_ref()
            .unwrap()
            .collect_batches_scoped(count, predicate, scope))
    }
}

impl OpLogBackend for OpLog {
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        OpLog::record_batch(self, operations)
    }

    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        OpLog::record_batch_scoped(self, operations, scope)
    }

    fn record_batch_scoped_if_no_transaction(
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

    fn last(&self) -> Result<Option<OpEntry>> {
        OpLog::last(self)
    }

    fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        OpLog::recent(self, count)
    }

    fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        OpLog::recent_batches(self, count)
    }

    fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        OpLog::recent_batches_scoped(self, count, scope)
    }

    fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        OpLog::undo_batches(self, count)
    }

    fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        OpLog::undo_batches_scoped(self, count, scope)
    }

    fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        OpLog::redo_batches(self, count)
    }

    fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
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
