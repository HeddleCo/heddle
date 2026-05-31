// SPDX-License-Identifier: Apache-2.0
//! Reference manager: threads, markers, HEAD, and packed refs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use objects::{
    error::{HeddleError, Result},
    fs_ops::remove_path_recursively,
    object::{ChangeId, MarkerName, ThreadName},
};

use super::{
    Head, RefExpectation, RefUpdate,
    backend::CoreRefBackend,
    format_change_id_text,
    packed_refs::PackedRefs,
    reconcile::{LoadRequest, Loaded, RefClass, RefCommitter, RefReconciler},
    ref_backend::RefBackend,
    resolve_refspec,
};
use crate::fs_atomic::sync_directory;

/// Sentinel meaning "no batch has been reconciled yet" — distinct from a real
/// `head_id` so the first read after a reconciler is injected always reconciles.
const WATERMARK_UNSET: u64 = u64::MAX;

/// Well-known refspec that resolves the heddle-internal pre-undo recovery
/// pointer (so `heddle goto .undo-recovery` works). It is UNSHADOWABLE by any
/// user marker or thread, in BOTH directions (heddle#305 r3):
///
/// - **Write side:** the leading `.` is rejected by [`validate_ref_name`], so a
///   user can never `marker create` / `thread` a ref with this name. The
///   recovery state therefore lives in a reserved namespace no user ref can
///   occupy.
/// - **Resolve side:** [`resolve_refspec`] routes this handle to the internal
///   recovery pointer BEFORE consulting user threads/markers, so no user ref
///   can intercept the advertised handle.
///
/// Invariant: an advertised handle for an internal ref must use a reserved form
/// that user-namespace names cannot take — never a bare user-namespace name.
///
/// [`validate_ref_name`]: super::name::validate_ref_name
pub const UNDO_RECOVERY_HANDLE: &str = ".undo-recovery";

/// Manager for references (threads, markers, HEAD).
pub struct RefManager {
    pub(crate) root: PathBuf,
    pub(crate) local_head: Option<PathBuf>,
    /// Oplog-backed reconciler (heddle#330 read chokepoint). `None` for the
    /// bootstrap/test path — then `reconciled_load` returns the plain cache,
    /// behaviourally identical to the pre-chokepoint code.
    reconciler: Option<Arc<dyn RefReconciler>>,
    /// Oplog-backed committer (heddle#330 write chokepoint). `None` for the
    /// bootstrap/test path — then `commit_and_publish` publishes without a
    /// record, like the pre-chokepoint code.
    committer: Option<Arc<dyn RefCommitter>>,
    /// Watermark of fully-materialized **local**-class batches (HEAD,
    /// undo-recovery) — `op_scope`-scoped. `WATERMARK_UNSET` until first reconcile.
    cached_local_generation: AtomicU64,
    /// Watermark of fully-materialized **shared**-class batches (thread, marker,
    /// remote-thread) — global across lanes.
    cached_shared_generation: AtomicU64,
}

impl RefManager {
    pub fn new(heddle_dir: impl AsRef<Path>) -> Self {
        Self {
            root: heddle_dir.as_ref().to_path_buf(),
            local_head: None,
            reconciler: None,
            committer: None,
            cached_local_generation: AtomicU64::new(WATERMARK_UNSET),
            cached_shared_generation: AtomicU64::new(WATERMARK_UNSET),
        }
    }

    pub fn with_local_head(mut self, path: PathBuf) -> Self {
        self.local_head = Some(path);
        self
    }

    /// Inject the oplog-backed reconciler (heddle#330 §2.2). Once set, every
    /// logical read funnels through [`RefManager::reconciled_load`] and
    /// reconciles against the committed oplog tail. Mirrors the
    /// [`with_local_head`](Self::with_local_head) builder shape.
    ///
    /// The class watermarks are seeded to the **current** generation: this
    /// handle trusts the already-published canonical cache as of open and
    /// reconciles only commits made *after* it — the load-bearing long-held
    /// handle cell (the daemon's `Arc<Repository>`, cid 3328112197) an
    /// open-time-only pass cannot reach. (Catching a *pre-open* crash lag is the
    /// job of the optional `Repository::open` eager pass, deferred here as the
    /// spike's stated optimization, not the guarantee.) Seeding to the current
    /// generation also keeps reconciliation from re-deriving long-since-deleted
    /// refs from old records in the un-migrated tree, where deletes do not all
    /// record yet.
    pub fn with_reconciler(mut self, reconciler: Arc<dyn RefReconciler>) -> Self {
        let generation = reconciler.generation();
        self.cached_local_generation
            .store(generation, Ordering::Release);
        self.cached_shared_generation
            .store(generation, Ordering::Release);
        self.reconciler = Some(reconciler);
        self
    }

    /// Inject the oplog-backed committer (heddle#330 §2.2 write chokepoint).
    /// Once set, [`commit_and_publish`](Self::commit_and_publish) appends the
    /// caller's ref-carrying records before publishing the ref batch.
    pub fn with_committer(mut self, committer: Arc<dyn RefCommitter>) -> Self {
        self.committer = Some(committer);
        self
    }

    /// THE write chokepoint (heddle#330 §2.2): commit the caller-supplied
    /// ref-carrying record batch (phase 4) **before** publishing the atomic ref
    /// batch (phase 5), record-before-publish, the whole batch published as one
    /// unit. `encoded_records` are opaque rmp-serde `OpRecord` bytes (so `refs`
    /// names no `oplog` type). The bare publish (temp→rename, via
    /// `update_refs_with_lock`) is reachable through this seam, never with a ref
    /// published ahead of its record. With no committer it degrades to a plain
    /// publish (bootstrap).
    ///
    /// **Invariant (cid 3329490978 / 3329490984): the oplog record and the ref
    /// publish commit together under the refs lock; a record exists iff its
    /// publish succeeded, and concurrent publishes to the same ref serialize
    /// record-and-publish as a unit.** The refs lock is taken FIRST, the ref
    /// expectations are validated (phase 3) BEFORE the record is appended (phase
    /// 4), and the publish (phase 5) follows under the same lock — so a failed
    /// expectation never leaks a record, and two concurrent callers can never
    /// append in one order and publish in another. (For `PgRefBackend` the single
    /// `pool.begin()…commit()` gives the same atomicity natively.)
    pub fn commit_and_publish(
        &self,
        encoded_records: &[Vec<u8>],
        ref_updates: &[RefUpdate],
        scope: Option<&str>,
    ) -> Result<()> {
        let lock = self.lock_refs()?;
        self.validate_commit_publish(ref_updates, &lock, || {
            // Phase 4 — the commit point: append the ref-carrying records only
            // after phase-3 validation has passed, under the held refs lock.
            let committed_for_reconcile = self.committer.is_some() && !encoded_records.is_empty();
            if let Some(committer) = self.committer.as_ref() {
                committer.commit_records(encoded_records, scope)?;
            }
            Ok(committed_for_reconcile)
        })
    }

    /// THE read chokepoint (heddle#330 §2.2): the sole path for a **logical
    /// read** to obtain ref data. The raw loaders
    /// (`read_change_id_at`/`read_head_state`/`try_read_ref_summary_index`/
    /// `*_from_storage`/`PackedRefs::load`) are reached from a logical read only
    /// from inside here — the maintenance path `pack_refs` is the one allowlisted
    /// non-logical caller. With no reconciler this is the plain raw load.
    fn reconciled_load(&self, req: LoadRequest) -> Result<Loaded> {
        let raw = self.raw_load(&req)?;
        let Some(reconciler) = self.reconciler.as_ref() else {
            return Ok(raw);
        };

        let tip = reconciler.generation();
        let watermark = match req.ref_class() {
            RefClass::Local => &self.cached_local_generation,
            RefClass::Shared => &self.cached_shared_generation,
        };
        let cached = watermark.load(Ordering::Acquire);
        if tip == cached {
            // No commit affecting this class since the last full materialization
            // ⇒ the canonical cache is current ⇒ no tail scan, no write.
            return Ok(raw);
        }

        // Lag possible: fold the committed tail newer than the watermark. The
        // reconcile is batch-atomic — it returns the re-materialization set for
        // every ref the lagged batches touched, which we publish so the
        // watermark can advance without leaving a batch sibling stale.
        let since = if cached == WATERMARK_UNSET { 0 } else { cached };
        let outcome = reconciler.reconcile(&req, raw, since)?;
        self.materialize(&outcome)?;
        watermark.store(tip, Ordering::Release);
        Ok(outcome.loaded)
    }

    /// Lazily re-publish (phase-5 materialization) the refs a reconcile found
    /// committed-but-unpublished — the records already exist, so this writes
    /// canonical only (never the oplog).
    ///
    /// **Authoritative-apply (cid 3329490981):** a committed record past the
    /// class watermark is authoritative over the live canonical, so a folded
    /// value is materialized when it CREATES a missing ref *or* UPDATES a stale
    /// present one (the crash-replayed update-to-existing case) — not
    /// fill-if-absent, which silently dropped a committed update to an
    /// already-existing ref. The folded set only ever holds refs touched by
    /// commits newer than the watermark, so applying it respects the
    /// two-watermark scoping and a ref with no recent committed record is never
    /// rewritten; a write equal to the canonical is skipped as a no-op. (The
    /// rare un-migrated case where an unrecorded direct write raced in *after*
    /// the commit is the residual the writers' record-first migration closes.)
    fn materialize(&self, outcome: &super::reconcile::ReconcileOutcome) -> Result<()> {
        let mut to_publish = Vec::new();
        for update in &outcome.republish {
            match update {
                RefUpdate::Thread { name, new, .. } => {
                    if self.raw_get_thread(name)? != *new {
                        to_publish.push(update.clone());
                    }
                }
                RefUpdate::Marker { name, new, .. } => {
                    if self.raw_get_marker(name)? != *new {
                        to_publish.push(update.clone());
                    }
                }
                RefUpdate::Head { new, .. } => {
                    if self.read_head_state()?.head != *new {
                        to_publish.push(update.clone());
                    }
                }
            }
        }
        if !to_publish.is_empty() {
            let lock = self.lock_refs()?;
            self.update_refs_with_lock(&to_publish, &lock)?;
        }
        for (remote, thread, value) in &outcome.remote_updates {
            if self.raw_get_remote_thread(remote, thread)? != *value {
                match value {
                    Some(state) => self.set_remote_thread_raw(remote, thread, state)?,
                    None => {
                        self.delete_remote_thread_raw(remote, thread)?;
                    }
                }
            }
        }
        if let Some(state) = &outcome.undo_recovery {
            let current = self.read_change_id_at(
                &self.undo_recovery_path(),
                "undo recovery",
                UNDO_RECOVERY_HANDLE,
            )?;
            if current.as_ref() != Some(state) {
                self.set_undo_recovery_raw(state)?;
            }
        }
        Ok(())
    }

    /// Request-scoped raw read — the private sub-step `reconciled_load` calls.
    /// Each arm touches exactly one raw loader for a point read (no whole-set
    /// scan on the hot path).
    fn raw_load(&self, req: &LoadRequest) -> Result<Loaded> {
        Ok(match req {
            LoadRequest::Head => Loaded::Head(self.read_head_state()?.head),
            LoadRequest::Thread(name) => Loaded::Point(self.raw_get_thread(name)?),
            LoadRequest::Marker(name) => Loaded::Point(self.raw_get_marker(name)?),
            LoadRequest::UndoRecovery => Loaded::Point(self.read_change_id_at(
                &self.undo_recovery_path(),
                "undo recovery",
                UNDO_RECOVERY_HANDLE,
            )?),
            LoadRequest::RemoteThread { remote, thread } => {
                Loaded::Point(self.raw_get_remote_thread(remote, thread)?)
            }
            LoadRequest::ThreadList => Loaded::ThreadList(self.raw_list_threads()?),
            LoadRequest::MarkerList => Loaded::MarkerList(self.raw_list_markers()?),
            LoadRequest::RemoteList => Loaded::RemoteList(self.raw_list_remotes()?),
            LoadRequest::RemoteThreadList { remote } => {
                Loaded::RemoteThreadList(self.raw_list_remote_threads(remote)?)
            }
        })
    }

    fn raw_get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        let path = self.thread_path(name)?;
        if let Some(id) = self.read_change_id_at(&path, "thread", name)? {
            return Ok(Some(id));
        }
        Ok(PackedRefs::load(&self.packed_refs_path())?.get_thread(name))
    }

    fn raw_get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        let path = self.marker_path(name)?;
        if let Some(id) = self.read_change_id_at(&path, "marker", name)? {
            return Ok(Some(id));
        }
        Ok(PackedRefs::load(&self.packed_refs_path())?.get_marker(name))
    }

    fn raw_get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        let path = self.remote_thread_path(remote, thread)?;
        self.read_change_id_at(&path, "remote thread", &format!("{}/{}", remote, thread))
    }

    fn raw_list_threads(&self) -> Result<Vec<ThreadName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.thread_names());
        }
        self.list_threads_from_storage()
    }

    fn raw_list_markers(&self) -> Result<Vec<MarkerName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.marker_names());
        }
        self.list_markers_from_storage()
    }

    fn raw_list_remotes(&self) -> Result<Vec<String>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.remote_names());
        }
        self.list_remotes_from_storage()
    }

    fn raw_list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>> {
        if let Some(summary) = self.try_read_ref_summary_index() {
            return Ok(summary.remote_thread_names(remote));
        }
        self.list_remote_threads_from_storage(remote)
    }

    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(self.threads_dir())?;
        std::fs::create_dir_all(self.markers_dir())?;
        std::fs::create_dir_all(self.remotes_dir())?;
        Ok(())
    }

    pub fn migrate_legacy_tracks(&self) -> Result<()> {
        let legacy_dir = self.legacy_tracks_dir();
        if !legacy_dir.exists() {
            return Ok(());
        }

        let threads_dir = self.threads_dir();
        if !threads_dir.exists() {
            std::fs::create_dir_all(self.refs_dir())?;
            std::fs::rename(&legacy_dir, &threads_dir)?;
            return Ok(());
        }

        let legacy_threads = self.list_refs_recursive(&legacy_dir, "")?;
        for name in legacy_threads {
            let legacy_path = self.legacy_track_path(&name)?;
            let thread_path = self.thread_path(&name)?;
            if thread_path.exists() {
                continue;
            }

            let parent = thread_path
                .parent()
                .ok_or_else(|| HeddleError::Config(format!("invalid thread path for {}", name)))?;
            std::fs::create_dir_all(parent)?;
            std::fs::rename(&legacy_path, &thread_path)?;
        }

        if legacy_dir.exists() {
            remove_path_recursively(&legacy_dir)?;
        }

        Ok(())
    }

    pub fn cleanup_stale_temps(&self) {
        let refs_dir = self.refs_dir();
        if let Ok(entries) = std::fs::read_dir(&refs_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.starts_with("tmp-"))
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    pub fn read_head(&self) -> Result<Head> {
        match self.reconciled_load(LoadRequest::Head)? {
            Loaded::Head(head) => Ok(head),
            _ => unreachable!("Head request yields Head"),
        }
    }

    pub fn write_head(&self, head: &Head) -> Result<()> {
        self.write_head_cas(RefExpectation::Any, head)
    }

    pub fn write_head_cas(&self, expected: RefExpectation<Head>, head: &Head) -> Result<()> {
        self.update_refs(&[RefUpdate::Head {
            expected,
            new: head.clone(),
        }])
    }

    pub fn get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        match self.reconciled_load(LoadRequest::Thread(name.clone()))? {
            Loaded::Point(id) => Ok(id),
            _ => unreachable!("Thread request yields Point"),
        }
    }

    pub fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<()> {
        self.set_thread_cas(name, RefExpectation::Any, state)
    }

    pub fn set_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Thread {
            name: name.clone(),
            expected,
            new: Some(*state),
        }])
    }

    pub fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        let state = self.get_thread(name)?;
        if state.is_some() {
            self.update_refs(&[RefUpdate::Thread {
                name: name.clone(),
                expected: RefExpectation::Any,
                new: None,
            }])?;
        }
        Ok(state)
    }

    pub fn delete_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Thread {
            name: name.clone(),
            expected,
            new: None,
        }])
    }

    pub fn list_threads(&self) -> Result<Vec<ThreadName>> {
        match self.reconciled_load(LoadRequest::ThreadList)? {
            Loaded::ThreadList(names) => Ok(names),
            _ => unreachable!("ThreadList request yields ThreadList"),
        }
    }

    pub fn get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        match self.reconciled_load(LoadRequest::Marker(name.clone()))? {
            Loaded::Point(id) => Ok(id),
            _ => unreachable!("Marker request yields Point"),
        }
    }

    pub fn create_marker(&self, name: &MarkerName, state: &ChangeId) -> Result<()> {
        self.set_marker_cas(name, RefExpectation::Missing, state)
    }

    pub fn set_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Marker {
            name: name.clone(),
            expected,
            new: Some(*state),
        }])
    }

    pub fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        let state = self.get_marker(name)?;
        if state.is_some() {
            self.delete_marker_cas(name, RefExpectation::Any)?;
        }
        Ok(state)
    }

    pub fn delete_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        self.update_refs(&[RefUpdate::Marker {
            name: name.clone(),
            expected,
            new: None,
        }])
    }

    pub fn list_markers(&self) -> Result<Vec<MarkerName>> {
        match self.reconciled_load(LoadRequest::MarkerList)? {
            Loaded::MarkerList(names) => Ok(names),
            _ => unreachable!("MarkerList request yields MarkerList"),
        }
    }

    /// Record the heddle-internal pre-undo recovery pointer (ORIG_HEAD-style:
    /// a single rolling ref each undo overwrites). Stored OUTSIDE the
    /// user-writable marker namespace so `marker create/delete` — and their
    /// undo inverses — can never collide with it. See
    /// [`UNDO_RECOVERY_HANDLE`] for the resolution handle.
    pub fn set_undo_recovery(&self, state: &ChangeId) -> Result<()> {
        self.set_undo_recovery_raw(state)
    }

    /// The raw canonical write for undo-recovery, with no oplog append — the
    /// phase-5 materialization sub-step (used by both the public setter and the
    /// reconciler's lazy re-publish).
    fn set_undo_recovery_raw(&self, state: &ChangeId) -> Result<()> {
        let _lock = self.lock_refs()?;
        self.write_string(
            &self.undo_recovery_path(),
            &super::format_change_id_text(state),
        )
    }

    /// Read the heddle-internal pre-undo recovery pointer, if one has been
    /// recorded. Returns `None` when no undo has run in this repo.
    pub fn get_undo_recovery(&self) -> Result<Option<ChangeId>> {
        match self.reconciled_load(LoadRequest::UndoRecovery)? {
            Loaded::Point(id) => Ok(id),
            _ => unreachable!("UndoRecovery request yields Point"),
        }
    }

    pub fn get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        match self.reconciled_load(LoadRequest::RemoteThread {
            remote: remote.to_string(),
            thread: thread.clone(),
        })? {
            Loaded::Point(id) => Ok(id),
            _ => unreachable!("RemoteThread request yields Point"),
        }
    }

    pub fn set_remote_thread(
        &self,
        remote: &str,
        thread: &ThreadName,
        state: &ChangeId,
    ) -> Result<()> {
        self.set_remote_thread_raw(remote, thread, state)
    }

    /// The raw canonical write for a remote-thread ref, with no oplog append —
    /// the phase-5 materialization sub-step.
    fn set_remote_thread_raw(
        &self,
        remote: &str,
        thread: &ThreadName,
        state: &ChangeId,
    ) -> Result<()> {
        let _lock = self.lock_refs()?;
        let path = self.remote_thread_path(remote, thread)?;
        let content = format_change_id_text(state);
        let parent = path.parent().ok_or_else(|| {
            HeddleError::Config(format!(
                "invalid remote thread path for {}/{}",
                remote, thread
            ))
        })?;
        std::fs::create_dir_all(parent)?;
        self.write_string(&path, &content)?;
        if self.rebuild_ref_summary_index_with_lock(&_lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        Ok(())
    }

    pub fn delete_remote_thread(
        &self,
        remote: &str,
        thread: &ThreadName,
    ) -> Result<Option<ChangeId>> {
        self.delete_remote_thread_raw(remote, thread)
    }

    /// The raw canonical delete for a remote-thread ref, with no oplog append.
    fn delete_remote_thread_raw(
        &self,
        remote: &str,
        thread: &ThreadName,
    ) -> Result<Option<ChangeId>> {
        let _lock = self.lock_refs()?;
        let state = self.raw_get_remote_thread(remote, thread)?;
        if state.is_some() {
            let path = self.remote_thread_path(remote, thread)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(HeddleError::from(e)),
            }
        }
        if self.rebuild_ref_summary_index_with_lock(&_lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        Ok(state)
    }

    pub fn list_remotes(&self) -> Result<Vec<String>> {
        match self.reconciled_load(LoadRequest::RemoteList)? {
            Loaded::RemoteList(names) => Ok(names),
            _ => unreachable!("RemoteList request yields RemoteList"),
        }
    }

    pub fn list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>> {
        match self.reconciled_load(LoadRequest::RemoteThreadList {
            remote: remote.to_string(),
        })? {
            Loaded::RemoteThreadList(names) => Ok(names),
            _ => unreachable!("RemoteThreadList request yields RemoteThreadList"),
        }
    }

    pub fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let lock = self.lock_refs()?;
        self.update_refs_with_lock(updates, &lock)
    }

    pub fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>> {
        resolve_refspec(
            refspec,
            || self.read_head(),
            |name| self.get_thread(&ThreadName::new(name)),
            |name| self.get_marker(&MarkerName::new(name)),
            || self.get_undo_recovery(),
        )
    }

    pub fn pack_refs(&self) -> Result<()> {
        let lock = self.lock_refs()?;
        let packed_path = self.packed_refs_path();
        let mut packed = PackedRefs::load(&packed_path)?;

        let threads = self.list_threads_from_storage()?;
        for name in &threads {
            let path = self.thread_path(name)?;
            if let Some(id) = self.read_change_id_at(&path, "thread", name)? {
                packed.set_thread(name, id);
            }
        }
        let markers = self.list_markers_from_storage()?;
        for name in &markers {
            let path = self.marker_path(name)?;
            if let Some(id) = self.read_change_id_at(&path, "marker", name)? {
                packed.set_marker(name, id);
            }
        }
        if !packed.is_empty() {
            packed.save(&packed_path)?;
            let packed_parent = packed_path
                .parent()
                .ok_or_else(|| HeddleError::Config("invalid packed-refs path".to_string()))?;
            sync_directory(packed_parent)?;
            for name in &threads {
                let path = self.thread_path(name)?;
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
            for name in &markers {
                let path = self.marker_path(name)?;
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        if self.rebuild_ref_summary_index_with_lock(&lock).is_err() {
            self.invalidate_ref_summary_index();
        }
        drop(lock);
        Ok(())
    }
}

impl CoreRefBackend for RefManager {
    type Error = HeddleError;

    fn read_head(&self) -> Result<Head> {
        RefManager::read_head(self)
    }
    fn write_head(&self, head: &Head) -> Result<()> {
        RefManager::write_head(self, head)
    }
    fn write_head_cas(&self, expected: RefExpectation<Head>, head: &Head) -> Result<()> {
        RefManager::write_head_cas(self, expected, head)
    }
    async fn get_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::get_thread(self, name)
    }
    fn set_thread(&self, name: &ThreadName, state: &ChangeId) -> Result<()> {
        RefManager::set_thread(self, name, state)
    }
    fn set_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        RefManager::set_thread_cas(self, name, expected, state)
    }
    fn delete_thread(&self, name: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::delete_thread(self, name)
    }
    fn delete_thread_cas(
        &self,
        name: &ThreadName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        RefManager::delete_thread_cas(self, name, expected)
    }
    fn list_threads(&self) -> Result<Vec<ThreadName>> {
        RefManager::list_threads(self)
    }
    async fn get_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        RefManager::get_marker(self, name)
    }
    async fn create_marker(&self, name: &MarkerName, state: &ChangeId) -> Result<()> {
        RefManager::create_marker(self, name, state)
    }
    fn set_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        RefManager::set_marker_cas(self, name, expected, state)
    }
    fn delete_marker(&self, name: &MarkerName) -> Result<Option<ChangeId>> {
        RefManager::delete_marker(self, name)
    }
    fn delete_marker_cas(
        &self,
        name: &MarkerName,
        expected: RefExpectation<ChangeId>,
    ) -> Result<()> {
        RefManager::delete_marker_cas(self, name, expected)
    }
    fn list_markers(&self) -> Result<Vec<MarkerName>> {
        RefManager::list_markers(self)
    }
    fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        RefManager::update_refs(self, updates)
    }
    async fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>> {
        RefManager::resolve(self, refspec)
    }
}

impl RefBackend for RefManager {
    fn get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::get_remote_thread(self, remote, thread)
    }
    fn set_remote_thread(&self, remote: &str, thread: &ThreadName, state: &ChangeId) -> Result<()> {
        RefManager::set_remote_thread(self, remote, thread, state)
    }
    fn delete_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>> {
        RefManager::delete_remote_thread(self, remote, thread)
    }
    fn list_remotes(&self) -> Result<Vec<String>> {
        RefManager::list_remotes(self)
    }
    fn list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>> {
        RefManager::list_remote_threads(self, remote)
    }
    fn commit_and_publish(
        &self,
        encoded_records: &[Vec<u8>],
        ref_updates: &[RefUpdate],
        scope: Option<&str>,
    ) -> Result<()> {
        RefManager::commit_and_publish(self, encoded_records, ref_updates, scope)
    }
    fn inspect_ref_summary_index(&self) -> Result<super::RefSummaryIndexInspection> {
        RefManager::inspect_ref_summary_index(self)
    }
    fn rebuild_ref_summary_index(&self) -> Result<super::RefSummaryIndexInspection> {
        RefManager::rebuild_ref_summary_index(self)
    }
    fn pack_refs(&self) -> Result<()> {
        RefManager::pack_refs(self)
    }
    fn cleanup_stale_temps(&self) {
        RefManager::cleanup_stale_temps(self)
    }
}
