// SPDX-License-Identifier: Apache-2.0
//! Core operation log logic — packed single-file format.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use objects::{
    error::{HeddleError, Result},
    fs_atomic::create_dir_all_durable,
    lock::{RepoLock, WriteLockGuard},
    object::Principal,
    sync::LockExt,
};

use super::{
    oplog_backend::OpLogBackend,
    oplog_types::{
        ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
        is_transaction_commit, is_transaction_commit_for, isolation_keys_for_record,
    },
    packed_oplog::{OplogRecoveryReport, PackedOpLog, PackedOpLogIndex, recover_oplog_at},
};

/// Operation log for tracking operations and enabling undo.
pub struct OpLog {
    pub(crate) root: PathBuf,
    cached: Mutex<Option<PackedOpLogIndex>>,
    actor: Arc<Principal>,
}

impl OpLog {
    /// Commit a batch through an independently durable artifact while holding
    /// the same oplog lock used for exact-once and isolation validation.
    ///
    /// `install` is invoked only after dedup/CAS validation and receives the
    /// current oplog head plus the canonical records (including the transaction
    /// marker). Once it returns, the operation is committed; the oplog rewrite
    /// is only a reconstructible materialized view.
    #[doc(hidden)]
    pub fn commit_reconstructible_batch_if_unchanged<T>(
        &self,
        mut operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        precondition: &IsolationPrecondition,
        install: impl FnOnce(u64, &[OpRecord]) -> Result<T>,
    ) -> Result<ReconstructibleCommitOutcome<T>> {
        let _lock = self.write_lock()?;
        let index = self.open_index_for_write()?;

        if index.transaction_commit(transaction_id)?.is_some() {
            return Ok(ReconstructibleCommitOutcome::AlreadyCommitted(
                index.committed_batch_records(transaction_id)?,
            ));
        }

        if !precondition.keys.is_empty() && index.head_id() != precondition.since_head_id {
            for entry in index.entries_after(precondition.since_head_id)? {
                let touched = isolation_keys_for_record(&entry.operation, entry.scope.as_deref());
                if let Some(key) = touched.intersection(&precondition.keys).next().cloned() {
                    return Ok(ReconstructibleCommitOutcome::IsolationConflict {
                        key,
                        since_head_id: precondition.since_head_id,
                        conflicting_entry_id: entry.id,
                    });
                }
            }
        }

        let op_count = operations.len() as u32;
        operations.push(OpRecord::TransactionCommit {
            transaction_id: transaction_id.to_string(),
            op_count,
        });
        let artifact = install(index.head_id(), &operations)?;

        let start_id = index.head_id() + 1;
        let timestamp = Utc::now();
        let scope_owned = scope.map(str::to_string);
        let entries =
            Self::build_entries(&self.actor, operations, start_id, timestamp, &scope_owned);
        match index.append_entries_reconstructible(&entries) {
            Ok(updated) => *self.cached.lock_or_poisoned() = Some(updated),
            Err(_error) => {
                // The pack install above is the commit point. A failed view
                // rewrite cannot turn a committed snapshot into an error;
                // invalidate the cache and let repository-open recovery replay
                // the artifact before the next process relies on the view.
                *self.cached.lock_or_poisoned() = None;
            }
        }
        Ok(ReconstructibleCommitOutcome::Committed(artifact))
    }

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
        create_dir_all_durable(&self.oplog_dir())?;
        create_dir_all_durable(&self.root.join("locks"))?;
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

    /// Validate the fixed on-disk format header without mutating the oplog.
    pub fn validate_current_format(&self) -> Result<()> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::validate_header(&path)?;
        }
        Ok(())
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

    /// Ensure the on-disk oplog is in the current format AND structurally intact,
    /// migrating an older one / salvaging a truncated one in place.
    ///
    /// **Hot-path discipline (perf/adopt residual O(N²)):** every logical ref
    /// read funnels through here via `head_id`/`load_cached`/`refresh_cached`, so
    /// this MUST stay cheap when the oplog is already healthy. The healthy path
    /// does two O(1) reads only —
    /// [`validate_header`](PackedOpLog::validate_header) reads the fixed header,
    /// and [`trailer_ok`](PackedOpLog::trailer_ok) seeks to EOF and reads the
    /// fixed footer prefix. Neither reads + parses the whole index.
    ///
    /// A prior version did `let _ = PackedOpLogIndex::open(&path)?` in the
    /// already-latest branch — a full file read + record-validation pass whose
    /// result was discarded. That turned a header read into an O(entries) parse
    /// on EVERY `head_id` call, so a read path that touches the oplog generation
    /// once per ref (e.g. the post-`adopt` verification/`status` walk's per-branch
    /// `get_thread`) reparsed the entire growing oplog N times ⇒ O(N²)
    /// (`adopt`/`status` of 800 refs reparsed a 142 KB oplog ~3.4k times and sat
    /// ~14 s). Dropping the discarded open removed that per-read full parse.
    ///
    /// But the discarded open had a load-bearing SIDE EFFECT: opening the index
    /// runs `PackedOpLogIndex::open`'s auto-salvage, which HEALS (and persists to
    /// disk) a truncated-but-header-valid oplog on a plain read. Header
    /// validation succeeds after truncation, so gating solely on it left the
    /// damaged file in place. Checking the trailer keeps the healthy path O(1)
    /// and routes damaged oplogs into the locked salvage branch.
    fn ensure_current_format(&self) -> Result<()> {
        let path = self.oplog_path();
        if !path.exists() {
            return Ok(());
        }
        // Early-return ONLY when the oplog is both current-format AND its index
        // trailer is intact. A truncated oplog keeps a valid header but loses
        // its footer, so it must fall through to salvage.
        PackedOpLog::validate_header(&path)?;
        if PackedOpLog::trailer_ok(&path)? {
            return Ok(());
        }
        let _lock = self.write_lock()?;
        PackedOpLog::ensure_current(&path)
    }

    /// Load from disk, bypassing cache (used after acquiring write lock).
    fn load_fresh_for_write(&self) -> Result<PackedOpLog> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::ensure_current(&path)?;
            PackedOpLog::load(&path)
        } else {
            Ok(PackedOpLog::new(path))
        }
    }

    fn open_index_for_write(&self) -> Result<PackedOpLogIndex> {
        let path = self.oplog_path();
        if path.exists() {
            PackedOpLog::ensure_current(&path)?;
        } else {
            PackedOpLog::new(path.clone()).save()?;
        }
        PackedOpLogIndex::open(&path)
    }

    /// Load from cache or disk (for read operations).
    fn load_cached(&self) -> Result<std::sync::MutexGuard<'_, Option<PackedOpLogIndex>>> {
        let guard = self.cached.lock_or_poisoned();
        if guard.is_some() {
            return Ok(guard);
        }
        drop(guard);

        self.ensure_current_format()?;
        let mut guard = self.cached.lock_or_poisoned();
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
        let mut guard = self.cached.lock_or_poisoned();
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

    /// Explicitly run the truncation-salvage path on this repository's oplog and
    /// report what was salvaged.
    ///
    /// This is the operator entrypoint behind `heddle oplog recover`. It runs
    /// the SAME recovery the silent auto-fallback in `load()`/`ensure_current()`
    /// would run — footer-guided first, then forward-greedy — quarantines the
    /// damaged original to `.corrupt`, writes the `.oplog.recovery` sidecar, and
    /// rebuilds `oplog.bin`. When the oplog is already healthy it returns an
    /// `already_healthy` report with no side effects.
    ///
    /// Takes the oplog write lock so the salvage cannot race a concurrent
    /// committer, and invalidates this handle's cache afterward.
    pub fn recover(&self) -> Result<OplogRecoveryReport> {
        let path = self.oplog_path();
        if !path.exists() {
            return Ok(OplogRecoveryReport::from_prior_sidecar(&path)
                .unwrap_or_else(OplogRecoveryReport::healthy));
        }
        let _lock = self.write_lock()?;
        let report = recover_oplog_at(&path)?;
        // Drop any cached (now-stale) index view so subsequent reads see the
        // rebuilt oplog.
        *self.cached.lock_or_poisoned() = None;
        Ok(report)
    }

    /// Get the last operation entry.
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

    /// Record a batch of operations.
    pub fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
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
            batch
                .entries
                .iter()
                .any(|entry| is_transaction_commit_for(&entry.operation, transaction_id))
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
        *self.cached.lock_or_poisoned() = Some(updated);

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
        *self.cached.lock_or_poisoned() = Some(updated);

        Ok(Some(ids))
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
        create_dir_all_durable(&self.oplog_dir())?;
        create_dir_all_durable(&self.root.join("locks"))?;
        let head_id = entries.last().map(|entry| entry.id).unwrap_or(0);
        let mut packed = PackedOpLog::new(self.oplog_path());
        packed.entries = entries;
        packed.head_id = head_id;
        packed.save()?;
        *self.cached.lock_or_poisoned() = Some(PackedOpLogIndex::open(&self.oplog_path())?);
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

    /// Return the exact batch carrying a committed transaction sentinel.
    /// The indexed lookup is unbounded by recency, so delayed land recovery
    /// cannot confuse an unrelated newer integration with its own batch.
    pub fn committed_batch(&self, transaction_id: &str) -> Result<Option<OpBatch>> {
        let guard = self.refresh_cached()?;
        guard.as_ref().unwrap().committed_batch(transaction_id)
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

        *self.cached.lock_or_poisoned() = Some(PackedOpLogIndex::open(&self.oplog_path())?);

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

#[doc(hidden)]
pub enum ReconstructibleCommitOutcome<T> {
    Committed(T),
    AlreadyCommitted(Vec<OpRecord>),
    IsolationConflict {
        key: super::oplog_types::IsolationKey,
        since_head_id: u64,
        conflicting_entry_id: u64,
    },
}

impl OpLogBackend for OpLog {
    fn record_batch_scoped(
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
        *self.cached.lock_or_poisoned() = Some(updated);

        Ok(ids)
    }

    fn record_batches_scoped(
        &self,
        groups: Vec<(Vec<OpRecord>, Option<&str>)>,
    ) -> Result<Vec<Vec<u64>>> {
        // Drop empty groups but remember their slot so the returned shape
        // matches the input 1:1 (an empty group consumes no id and no
        // batch).
        if groups.iter().all(|(ops, _)| ops.is_empty()) {
            return Ok(groups.iter().map(|_| Vec::new()).collect());
        }

        let _lock = self.write_lock()?;
        // Reload from disk to catch any writes from other processes — same
        // freshness contract as the single-batch path.
        let index = self.open_index_for_write()?;

        let timestamp = Utc::now();
        // One id space across the whole call: ids stay globally monotonic and
        // sequential across all groups (so replay order == emit order), while
        // each group keeps a distinct `batch_id` (= the id of its first
        // entry) so it remains an independent undo/redo unit.
        let mut next_id = index.head_id() + 1;
        let mut all_entries: Vec<OpEntry> = Vec::new();
        let mut result: Vec<Vec<u64>> = Vec::with_capacity(groups.len());

        for (operations, scope) in groups {
            if operations.is_empty() {
                result.push(Vec::new());
                continue;
            }
            let scope_owned = scope.map(str::to_string);
            let entries =
                Self::build_entries(&self.actor, operations, next_id, timestamp, &scope_owned);
            let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
            next_id += entries.len() as u64;
            all_entries.extend(entries);
            result.push(ids);
        }

        // Single full-log rewrite for every batch in the call — this is the
        // O(N²)→O(N) win for the importer's reflog→oplog emit.
        let updated = index.append_entries(&all_entries)?;
        *self.cached.lock_or_poisoned() = Some(updated);

        Ok(result)
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
        *self.cached.lock_or_poisoned() = Some(updated);

        Ok(ConditionalCommitOutcome::Committed(ids))
    }

    fn last(&self) -> Result<Option<OpEntry>> {
        let guard = self.load_cached()?;
        guard.as_ref().unwrap().last_entry()
    }

    fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        let guard = self.load_cached()?;
        guard.as_ref().unwrap().recent_entries(count)
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
        self.update_batch_undone_state(batch, true)
    }

    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        self.update_batch_undone_state(batch, false)
    }

    fn coalesce_batches(&self, primary_batch_id: u64, secondary_batch_id: u64) -> Result<OpBatch> {
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
        *self.cached.lock_or_poisoned() = Some(PackedOpLogIndex::open(&self.oplog_path())?);

        Ok(OpBatch {
            id: primary_batch_id,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn snapshot_record() -> OpRecord {
        let state = crate::oplog::fresh_state_id();
        OpRecord::Snapshot {
            new_state: state,
            prev_head: None,
            head: Some(state),
            thread: None,
        }
    }

    /// Regression test for the perf/adopt residual-O(N²) change (PR #830):
    /// dropping the discarded `PackedOpLogIndex::open` from
    /// `ensure_current_format`'s already-latest branch removed a load-bearing
    /// side effect — the open's auto-salvage that heals a truncated-but-header-
    /// valid oplog on a plain READ. Header validation succeeds after truncation,
    /// so the healthy hot path stayed fast but damaged oplogs stopped
    /// self-healing on read.
    ///
    /// Pairing header validation with `trailer_ok` routes truncated oplogs into
    /// salvage. A plain `head_id()` read must repair `oplog.bin` on disk.
    #[test]
    fn truncated_oplog_self_heals_on_plain_read() {
        let tmp = TempDir::new().unwrap();
        let oplog = OpLog::new_unattributed(tmp.path());
        oplog.init().unwrap();
        // Seed several real batches so there is an entry stream to salvage.
        for _ in 0..3 {
            oplog.record_batch(vec![snapshot_record()]).unwrap();
        }

        let path = oplog.oplog_path();
        let bytes = std::fs::read(&path).unwrap();

        // Truncation that destroys the trailing index/footer but keeps the
        // fixed header intact — the exact damage the CLI `oplog_recover` fixture
        // reproduces. Cut at 60% so entries remain but the footer is gone.
        let cut = bytes.len() * 6 / 10;
        std::fs::write(&path, &bytes[..cut]).unwrap();

        // The header survives truncation, so validation still succeeds — this
        // is precisely why gating on it alone missed the damage.
        PackedOpLog::validate_header(&path)
            .expect("truncated oplog keeps a valid current-format header");
        // ...but the footer/trailer is destroyed, so the cheap O(1) integrity
        // predicate correctly reports damage.
        assert!(
            !PackedOpLog::trailer_ok(&path).unwrap(),
            "truncated oplog has a destroyed trailer"
        );

        // A plain READ (no mutation) must self-heal the on-disk oplog: this is
        // the side effect the perf change dropped and this fix restores. A fresh
        // handle avoids any in-memory cache masking the on-disk state.
        let reader = OpLog::new_unattributed(tmp.path());
        reader.head_id().expect("read salvages the truncated oplog");

        // On-disk file is repaired: the footer is back and the trailer check
        // passes again.
        assert!(
            PackedOpLog::trailer_ok(&path).unwrap(),
            "on-disk oplog trailer must be restored after a plain read"
        );
        assert!(
            PackedOpLogIndex::open(&path).is_ok(),
            "repaired oplog must open + validate cleanly"
        );
        // The damaged original was quarantined to `.corrupt`, proving the
        // salvage path (not just an in-memory patch) ran and persisted.
        assert!(
            path.with_file_name("oplog.bin.corrupt").exists(),
            "salvage must quarantine the damaged original to .corrupt"
        );
    }

    /// A healthy current-format oplog passes both currency and trailer checks,
    /// so `ensure_current_format`'s hot path stays O(1) (no salvage, no rewrite).
    #[test]
    fn healthy_oplog_passes_trailer_check() {
        let tmp = TempDir::new().unwrap();
        let oplog = OpLog::new_unattributed(tmp.path());
        oplog.init().unwrap();
        oplog.record_batch(vec![snapshot_record()]).unwrap();

        let path = oplog.oplog_path();
        PackedOpLog::validate_header(&path).unwrap();
        assert!(
            PackedOpLog::trailer_ok(&path).unwrap(),
            "a healthy oplog's trailer must be intact"
        );

        let before = std::fs::read(&path).unwrap();
        // A plain read must NOT rewrite a healthy oplog.
        oplog.head_id().unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "healthy oplog must not be rewritten on read");
        assert!(
            !path.with_file_name("oplog.bin.corrupt").exists(),
            "healthy oplog must not be quarantined"
        );
    }
}
