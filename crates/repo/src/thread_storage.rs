// SPDX-License-Identifier: Apache-2.0
//! Thread storage and lifecycle management.

use objects::store::ObjectStore;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use objects::{
    lock::RepoLock,
    object::ChangeId,
    store::{HeddleError, Result},
};

use crate::{
    thread_model::{
        EphemeralMarker, ThreadConfidenceSummary, ThreadFreshness, ThreadImpactCategory,
        ThreadIntegrationPolicy, ThreadMode, ThreadRecord, ThreadRuntimeOverlay, ThreadState,
        ThreadVerificationSummary, ThreadView,
    },
    thread_record_store::{FilesystemThreadRecordStore, ThreadRecordStore},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub id: String,
    pub thread: String,
    #[serde(default)]
    pub target_thread: Option<String>,
    #[serde(default)]
    pub parent_thread: Option<String>,
    pub mode: ThreadMode,
    pub state: ThreadState,
    pub base_state: String,
    pub base_root: String,
    #[serde(default)]
    pub current_state: Option<String>,
    #[serde(default)]
    pub merged_state: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub execution_path: PathBuf,
    #[serde(default)]
    pub materialized_path: Option<PathBuf>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    #[serde(default)]
    pub impact_categories: Vec<ThreadImpactCategory>,
    #[serde(default)]
    pub heavy_impact_paths: Vec<String>,
    #[serde(default)]
    pub promotion_suggested: bool,
    #[serde(default = "default_freshness")]
    pub freshness: ThreadFreshness,
    #[serde(default)]
    pub verification_summary: ThreadVerificationSummary,
    #[serde(default)]
    pub confidence_summary: ThreadConfidenceSummary,
    #[serde(default)]
    pub integration_policy_result: ThreadIntegrationPolicy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Optional ephemeral-thread marker (W1). Preserved across record/Thread
    /// round-trips so the auto-collapse sweep can find it.
    #[serde(default)]
    pub ephemeral: Option<EphemeralMarker>,
    /// Mirror of [`ThreadRecord::auto`]. See that field for semantics.
    /// Defaults to `false` for back-compat with pre-2.2 records.
    #[serde(default)]
    pub auto: bool,
    /// Mirror of [`ThreadRecord::shared_target_dir`]. See that field for
    /// semantics. `None` for threads that use cargo's default per-checkout
    /// `target/`. Defaults to `None` for back-compat with records written
    /// before the field existed.
    #[serde(default)]
    pub shared_target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct ThreadWorkspaceState {
    #[serde(default)]
    execution_path: PathBuf,
    #[serde(default)]
    materialized_path: Option<PathBuf>,
}

impl Thread {
    pub fn to_record(&self) -> ThreadRecord {
        ThreadRecord {
            id: self.id.clone(),
            thread: self.thread.clone(),
            target_thread: self.target_thread.clone(),
            parent_thread: self.parent_thread.clone(),
            mode: self.mode.clone(),
            state: self.state.clone(),
            base_state: self.base_state.clone(),
            base_root: self.base_root.clone(),
            current_state: self.current_state.clone(),
            merged_state: self.merged_state.clone(),
            task: self.task.clone(),
            changed_paths: self.changed_paths.clone(),
            impact_categories: self.impact_categories.clone(),
            heavy_impact_paths: self.heavy_impact_paths.clone(),
            promotion_suggested: self.promotion_suggested,
            freshness: self.freshness.clone(),
            verification_summary: self.verification_summary.clone(),
            confidence_summary: self.confidence_summary.clone(),
            integration_policy_result: self.integration_policy_result.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            ephemeral: self.ephemeral.clone(),
            auto: self.auto,
            shared_target_dir: self.shared_target_dir.clone(),
        }
    }

    pub fn from_record(record: ThreadRecord) -> Self {
        Self {
            id: record.id.clone(),
            thread: record.thread,
            target_thread: record.target_thread,
            parent_thread: record.parent_thread,
            mode: record.mode,
            state: record.state,
            base_state: record.base_state,
            base_root: record.base_root,
            current_state: record.current_state,
            merged_state: record.merged_state,
            task: record.task,
            execution_path: PathBuf::new(),
            materialized_path: None,
            changed_paths: record.changed_paths,
            impact_categories: record.impact_categories,
            heavy_impact_paths: record.heavy_impact_paths,
            promotion_suggested: record.promotion_suggested,
            freshness: record.freshness,
            verification_summary: record.verification_summary,
            confidence_summary: record.confidence_summary,
            integration_policy_result: record.integration_policy_result,
            created_at: record.created_at,
            updated_at: record.updated_at,
            ephemeral: record.ephemeral,
            auto: record.auto,
            shared_target_dir: record.shared_target_dir,
        }
    }

    pub fn to_view(&self, runtime: ThreadRuntimeOverlay, is_current: bool) -> ThreadView {
        ThreadView::from_record(self.to_record(), runtime, is_current)
    }

    fn workspace_state(&self) -> ThreadWorkspaceState {
        ThreadWorkspaceState {
            execution_path: self.execution_path.clone(),
            materialized_path: self.materialized_path.clone(),
        }
    }
}

impl ThreadWorkspaceState {
    fn apply_to_thread(&self, thread: &mut Thread) {
        thread.execution_path = self.execution_path.clone();
        thread.materialized_path = self.materialized_path.clone();
    }
}

pub type SyncedThreadMetadata = ThreadRecord;

impl SyncedThreadMetadata {
    pub fn from_record(
        repo: &crate::Repository,
        record: &ThreadRecord,
        current_state_override: Option<ChangeId>,
    ) -> Result<Self> {
        let mut record = record.clone();
        let resolve_full = |spec: &str| -> Result<String> {
            Ok(repo
                .resolve_state(spec)?
                .map(|id| id.to_string_full())
                .unwrap_or_else(|| spec.to_string()))
        };
        record.base_state = resolve_full(&record.base_state)?;
        record.current_state = match current_state_override {
            Some(id) => Some(id.to_string_full()),
            None => record
                .current_state
                .as_deref()
                .map(resolve_full)
                .transpose()?,
        };
        record.merged_state = record
            .merged_state
            .as_deref()
            .map(resolve_full)
            .transpose()?;
        record.base_root = repo
            .resolve_state(&record.base_state)?
            .and_then(|id| repo.store().get_state(&id).ok().flatten())
            .map(|state| state.tree.to_hex())
            .unwrap_or_else(|| record.base_root.clone());
        Ok(record)
    }

    pub fn from_thread(
        repo: &crate::Repository,
        thread: &Thread,
        current_state_override: Option<ChangeId>,
    ) -> Result<Self> {
        Self::from_record(repo, &thread.to_record(), current_state_override)
    }

    pub fn current_state_change_id(&self, repo: &crate::Repository) -> Result<Option<ChangeId>> {
        match self.current_state.as_deref() {
            Some(state) => Ok(repo.resolve_state(state)?),
            None => Ok(None),
        }
    }
}

fn default_freshness() -> ThreadFreshness {
    ThreadFreshness::Unknown
}

#[derive(Debug, Clone)]
pub struct ThreadManager {
    record_store: FilesystemThreadRecordStore,
    workspace_store: FilesystemThreadRecordStore,
    lock_path: PathBuf,
}

impl ThreadManager {
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            record_store: FilesystemThreadRecordStore::new(heddle_dir.join("thread_records")),
            workspace_store: FilesystemThreadRecordStore::new(heddle_dir.join("thread_workspaces")),
            lock_path: heddle_dir.join("thread_records").join(".lock"),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.lock_path.clone()
    }

    fn write_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path())
            .write()
            .map_err(|err| HeddleError::Config(format!("failed to acquire thread lock: {err}")))
    }

    fn save_record_file(&self, record: &ThreadRecord) -> Result<()> {
        self.record_store.save_value(&record.id, record)
    }

    fn save_workspace_file(&self, thread_id: &str, workspace: &ThreadWorkspaceState) -> Result<()> {
        self.workspace_store.save_value(thread_id, workspace)
    }

    fn load_record_file(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        self.record_store.load_value(thread_id)
    }

    fn load_workspace_file(&self, thread_id: &str) -> Result<Option<ThreadWorkspaceState>> {
        self.workspace_store.load_value(thread_id)
    }

    fn list_record_files(&self) -> Result<Vec<ThreadRecord>> {
        self.record_store.list_values()
    }

    fn hydrate_thread_from_record(&self, record: ThreadRecord) -> Result<Thread> {
        let mut thread = Thread::from_record(record.clone());
        if let Some(workspace) = self.load_workspace_file(&record.id)? {
            workspace.apply_to_thread(&mut thread);
        }
        Ok(thread)
    }

    pub fn save(&self, thread: &Thread) -> Result<()> {
        let _lock = self.write_lock()?;
        self.save_record_file(&thread.to_record())?;
        self.save_workspace_file(&thread.id, &thread.workspace_state())
    }

    pub fn load(&self, thread_id: &str) -> Result<Option<Thread>> {
        self.load_record_file(thread_id)?
            .map(|record| self.hydrate_thread_from_record(record))
            .transpose()
    }

    pub fn list(&self) -> Result<Vec<Thread>> {
        let mut threads: Vec<Thread> = self
            .list_record_files()?
            .into_iter()
            .map(|record| self.hydrate_thread_from_record(record))
            .collect::<Result<Vec<_>>>()?;
        threads.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(threads)
    }

    pub fn find_by_thread(&self, thread: &str) -> Result<Option<Thread>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|thread_entry| thread_entry.thread == thread)
            .max_by_key(|thread_entry| thread_entry.updated_at))
    }

    pub fn find_by_execution_root(&self, root: &Path) -> Result<Option<Thread>> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Ok(self.list()?.into_iter().find(|thread| {
            thread
                .execution_path
                .canonicalize()
                .unwrap_or_else(|_| thread.execution_path.clone())
                == canonical
                || thread
                    .materialized_path
                    .as_ref()
                    .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()) == canonical)
                    .unwrap_or(false)
        }))
    }

    pub fn delete(&self, thread_id: &str) -> Result<()> {
        let _lock = self.write_lock()?;
        self.record_store.delete_value(thread_id)?;
        self.workspace_store.delete_value(thread_id)
    }

    /// Converge the persisted state for thread `name` to `target`: delete EVERY
    /// record currently filed under `name` whose record id differs from the
    /// target's, then (if `Some`) ensure `target` is saved. Post-condition: the
    /// set of records with `.thread == name` is exactly `{target}` (or empty),
    /// so [`find_by_thread`](Self::find_by_thread) returns `target` REGARDLESS of
    /// what (possibly unknown-id, newer-timestamped) record a rolled-back forward
    /// left behind. Correct-by-construction: the leaked record's id never has to
    /// be known, because every non-target record under the name is dropped.
    pub fn restore_to_snapshot(&self, name: &str, target: Option<&Thread>) -> Result<()> {
        // Acquire the write lock ONCE and perform the whole enumerate→delete→save
        // under that single guard, via the PRIVATE file-level helpers. The public
        // `list`/`delete`/`save` each re-acquire `write_lock()`, which is NOT
        // re-entrant (flock on a second FD in the same process blocks), so calling
        // them here would deadlock — and a concurrent same-name writer between an
        // unlocked `list()` snapshot and the deletes could leak a record that
        // survives the converge.
        let _lock = self.write_lock()?;
        let keep = target.map(|t| t.id.as_str());
        for record in self.list_record_files()? {
            if record.thread == name && Some(record.id.as_str()) != keep {
                self.record_store.delete_value(&record.id)?;
                self.workspace_store.delete_value(&record.id)?;
            }
        }
        if let Some(target) = target {
            self.save_record_file(&target.to_record())?;
            self.save_workspace_file(&target.id, &target.workspace_state())?;
        }
        Ok(())
    }

    pub fn load_record(&self, record_id: &str) -> Result<Option<ThreadRecord>> {
        self.load_record_file(record_id)
    }

    pub fn save_record(&self, record: &ThreadRecord) -> Result<()> {
        let _lock = self.write_lock()?;
        self.save_record_file(record)
    }

    pub fn list_records(&self) -> Result<Vec<ThreadRecord>> {
        let mut records: Vec<ThreadRecord> = self.list_record_files()?;
        records.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(records)
    }

    pub fn find_record_by_thread(&self, thread: &str) -> Result<Option<ThreadRecord>> {
        ThreadRecordStore::find_record_by_thread(self, thread)
    }

    pub fn find_synced_record_by_thread(
        &self,
        repo: &crate::Repository,
        thread: &str,
        current_state_override: Option<ChangeId>,
    ) -> Result<Option<SyncedThreadMetadata>> {
        self.find_record_by_thread(thread)?
            .map(|record| SyncedThreadMetadata::from_record(repo, &record, current_state_override))
            .transpose()
    }

    pub fn delete_record(&self, record_id: &str) -> Result<()> {
        let _lock = self.write_lock()?;
        self.record_store.delete_value(record_id)
    }

    /// Encode the `Thread` record matching `thread_name` to opaque
    /// rmp-serde bytes for inclusion in `OpRecord::ThreadCreateV2`'s
    /// `manager_snapshot` field. Returns `Ok(None)` when no record
    /// exists for that thread (the caller doesn't have a record to
    /// snapshot — e.g. `cmd_start --path` writes the record only after
    /// materialization, and the rename batch's new-name arm never has
    /// one). heddle#23 r2.
    ///
    /// The encoding is opaque to the `oplog` crate: it stores the bytes
    /// without interpreting them. The shape is `Thread`'s serde form,
    /// which has `#[serde(default)]` on every optional field, so
    /// records written by future versions of heddle remain decodable
    /// by older readers and vice versa.
    pub fn snapshot_thread_record(&self, thread_name: &str) -> Result<Option<Vec<u8>>> {
        let Some(thread) = self.find_by_thread(thread_name)? else {
            return Ok(None);
        };
        let bytes = rmp_serde::to_vec_named(&thread).map_err(|e| {
            HeddleError::Serialization(format!(
                "encode thread record snapshot for '{}': {}",
                thread_name, e
            ))
        })?;
        Ok(Some(bytes))
    }

    /// Decode and persist a `Thread` record from rmp-serde bytes
    /// produced by `snapshot_thread_record`. Used by `heddle redo` of a
    /// `ThreadCreateV2` to restore the record body that undo destroyed
    /// (heddle#23 r2 Codex P1, mirroring the FastForwardV2 pattern from
    /// heddle#99 r2 — record what redo needs).
    ///
    /// Returns the restored `Thread` for callers that want to inspect
    /// it (e.g. for stderr summaries). An empty/invalid snapshot
    /// surfaces as `HeddleError::Other` — the redo arm logs and falls
    /// back to ref-only restore rather than failing the whole batch.
    pub fn restore_thread_record_from_snapshot(&self, bytes: &[u8]) -> Result<Thread> {
        let thread: Thread = rmp_serde::from_slice(bytes).map_err(|e| {
            HeddleError::Serialization(format!("decode thread record snapshot: {}", e))
        })?;
        self.save(&thread)?;
        Ok(thread)
    }
}

impl ThreadRecordStore for ThreadManager {
    fn load_record(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        ThreadManager::load_record(self, thread_id)
    }

    fn save_record(&self, record: &ThreadRecord) -> Result<()> {
        ThreadManager::save_record(self, record)
    }

    fn list_records(&self) -> Result<Vec<ThreadRecord>> {
        ThreadManager::list_records(self)
    }

    fn delete_record(&self, thread_id: &str) -> Result<()> {
        ThreadManager::delete_record(self, thread_id)
    }
}

#[derive(Debug, Clone)]
pub struct SyncedThreadMetadataStore {
    store: FilesystemThreadRecordStore,
}

impl SyncedThreadMetadataStore {
    pub fn new(heddle_dir: &Path) -> Self {
        Self {
            store: FilesystemThreadRecordStore::new(heddle_dir.join("hosted_threads")),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.store.lock_path()
    }

    fn write_lock(&self) -> Result<objects::lock::WriteLockGuard> {
        RepoLock::at(self.lock_path()).write().map_err(|err| {
            HeddleError::Config(format!("failed to acquire thread metadata lock: {err}"))
        })
    }

    fn save_metadata_file(&self, metadata: &SyncedThreadMetadata) -> Result<()> {
        self.store.save_value(&metadata.thread, metadata)
    }

    fn load_metadata_file(&self, thread: &str) -> Result<Option<SyncedThreadMetadata>> {
        self.store.load_value(thread)
    }

    fn list_metadata_files(&self) -> Result<Vec<SyncedThreadMetadata>> {
        self.store.list_values()
    }

    pub fn save(&self, metadata: &SyncedThreadMetadata) -> Result<()> {
        let _lock = self.write_lock()?;
        self.save_metadata_file(metadata)
    }

    pub fn load(&self, thread: &str) -> Result<Option<SyncedThreadMetadata>> {
        self.load_metadata_file(thread)
    }

    pub fn list(&self) -> Result<Vec<SyncedThreadMetadata>> {
        let mut out = self.list_metadata_files()?;
        out.sort_by(|a, b| a.thread.cmp(&b.thread));
        Ok(out)
    }

    pub fn load_record(&self, thread: &str) -> Result<Option<ThreadRecord>> {
        self.load_metadata_file(thread)
    }

    pub fn save_record(&self, record: &ThreadRecord) -> Result<()> {
        let _lock = self.write_lock()?;
        self.store.save_value(&record.thread, record)
    }

    pub fn list_records(&self) -> Result<Vec<ThreadRecord>> {
        let mut records: Vec<ThreadRecord> = self.store.list_values()?;
        records.sort_by(|a, b| a.thread.cmp(&b.thread));
        Ok(records)
    }

    pub fn delete_record(&self, thread: &str) -> Result<()> {
        let _lock = self.write_lock()?;
        self.store.delete_value(thread)
    }
}

impl ThreadRecordStore for SyncedThreadMetadataStore {
    fn load_record(&self, thread_id: &str) -> Result<Option<ThreadRecord>> {
        SyncedThreadMetadataStore::load_record(self, thread_id)
    }

    fn save_record(&self, record: &ThreadRecord) -> Result<()> {
        SyncedThreadMetadataStore::save_record(self, record)
    }

    fn list_records(&self) -> Result<Vec<ThreadRecord>> {
        SyncedThreadMetadataStore::list_records(self)
    }

    fn delete_record(&self, thread_id: &str) -> Result<()> {
        SyncedThreadMetadataStore::delete_record(self, thread_id)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;

    fn sample_thread() -> Thread {
        Thread {
            id: "thread-1".to_string(),
            thread: "feature/thread-1".to_string(),
            target_thread: Some("main".to_string()),
            parent_thread: None,
            mode: ThreadMode::Solid,
            state: ThreadState::Active,
            base_state: "abc123".to_string(),
            base_root: "def456".to_string(),
            current_state: Some("abc123".to_string()),
            merged_state: None,
            task: Some("implement thing".to_string()),
            execution_path: PathBuf::from("/tmp/work"),
            materialized_path: Some(PathBuf::from("/tmp/materialized")),
            changed_paths: vec!["src/lib.rs".to_string()],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: ThreadFreshness::Current,
            verification_summary: ThreadVerificationSummary::default(),
            confidence_summary: ThreadConfidenceSummary::default(),
            integration_policy_result: ThreadIntegrationPolicy::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    #[test]
    fn thread_manager_round_trips_canonical_record_and_workspace() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let thread = sample_thread();

        manager.save(&thread).unwrap();
        let record = manager.load_record(&thread.id).unwrap().unwrap();
        let hydrated = manager.load(&thread.id).unwrap().unwrap();

        assert_eq!(record.id, thread.id);
        assert_eq!(record.thread, thread.thread);
        assert_eq!(record.base_state, thread.base_state);
        assert_eq!(record.task, thread.task);
        assert_eq!(hydrated.execution_path, thread.execution_path);
        assert_eq!(hydrated.materialized_path, thread.materialized_path);
    }

    /// Item 2.2 of the heddle 6→8 plan adds an `auto` flag to
    /// distinguish harness-created threads from user-created ones.
    /// The flag must round-trip through both storage halves and
    /// default to `false` when missing on disk (back-compat for
    /// records written before the field existed).
    #[test]
    fn thread_manager_round_trips_auto_flag() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let mut thread = sample_thread();
        thread.id = "thread-auto".to_string();
        thread.thread = "feature/thread-auto".to_string();
        thread.auto = true;

        manager.save(&thread).unwrap();
        let record = manager.load_record(&thread.id).unwrap().unwrap();
        let hydrated = manager.load(&thread.id).unwrap().unwrap();

        assert!(
            record.auto,
            "auto flag must persist on the canonical record"
        );
        assert!(
            hydrated.auto,
            "auto flag must round-trip back into the hydrated Thread"
        );
    }

    /// `restore_to_snapshot` is the converge primitive every thread-record undo
    /// restore routes through (heddle#355 r6, close-the-class). A non-atomic
    /// forward can write a record under a DIFFERENT id with a NEWER `updated_at`,
    /// which `find_by_thread`'s `max_by_key(updated_at)` would then return. The
    /// converge must delete that leaked record so the post-condition holds: the
    /// records filed under `name` are exactly `{target}`.
    #[test]
    fn restore_to_snapshot_drops_leaked_newer_id_record() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let name = "feature/converge";

        let mut prev = sample_thread();
        prev.id = "rec-prev".to_string();
        prev.thread = name.to_string();
        prev.updated_at = Utc::now();
        manager.save(&prev).unwrap();

        // A forward wrote a SECOND record under the same name — different id,
        // NEWER timestamp — so `find_by_thread` returns the leaked record.
        let mut leaked = sample_thread();
        leaked.id = "rec-leaked".to_string();
        leaked.thread = name.to_string();
        leaked.updated_at = prev.updated_at + chrono::Duration::seconds(10);
        manager.save(&leaked).unwrap();
        assert_eq!(
            manager.find_by_thread(name).unwrap().unwrap().id,
            "rec-leaked",
            "precondition: newer leaked record shadows prev"
        );

        manager.restore_to_snapshot(name, Some(&prev)).unwrap();

        assert_eq!(
            manager.find_by_thread(name).unwrap().unwrap().id,
            "rec-prev",
            "converge returns find_by_thread to the target, not the leak"
        );
        let under_name: Vec<_> = manager
            .list()
            .unwrap()
            .into_iter()
            .filter(|t| t.thread == name)
            .collect();
        assert_eq!(
            under_name.len(),
            1,
            "exactly the target remains filed under the name"
        );
    }

    /// Lock-atomic converge over MULTIPLE same-name records (heddle#355 r7, Codex
    /// cid 3331420787). With the target PLUS two other same-name records (one
    /// newer-timestamped, so it would shadow the target via
    /// `find_by_thread`'s `max_by_key(updated_at)`), the converge enumerates and
    /// deletes them all under a SINGLE write lock via the private file-level
    /// helpers — never re-acquiring the (non-re-entrant) lock through the public
    /// `list`/`delete`/`save`. Both storage halves (record + workspace file) of
    /// each dropped record must be gone.
    #[test]
    fn restore_to_snapshot_converges_multiple_same_name_records() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let name = "feature/multi";

        let mut target = sample_thread();
        target.id = "rec-target".to_string();
        target.thread = name.to_string();
        target.updated_at = Utc::now();
        manager.save(&target).unwrap();

        let mut older = sample_thread();
        older.id = "rec-older".to_string();
        older.thread = name.to_string();
        older.updated_at = target.updated_at - chrono::Duration::seconds(30);
        manager.save(&older).unwrap();

        let mut newer = sample_thread();
        newer.id = "rec-newer".to_string();
        newer.thread = name.to_string();
        newer.updated_at = target.updated_at + chrono::Duration::seconds(30);
        manager.save(&newer).unwrap();
        assert_eq!(
            manager.find_by_thread(name).unwrap().unwrap().id,
            "rec-newer",
            "precondition: the newer same-name record shadows the target"
        );

        manager.restore_to_snapshot(name, Some(&target)).unwrap();

        let under_name: Vec<_> = manager
            .list()
            .unwrap()
            .into_iter()
            .filter(|t| t.thread == name)
            .collect();
        assert_eq!(under_name.len(), 1, "exactly the target remains under the name");
        assert_eq!(under_name[0].id, "rec-target");
        assert_eq!(
            manager.find_by_thread(name).unwrap().unwrap().id,
            "rec-target",
            "converge returns find_by_thread to the target"
        );
        // Both storage halves of every dropped record are gone.
        for dropped in ["rec-older", "rec-newer"] {
            assert!(
                manager.load_record_file(dropped).unwrap().is_none(),
                "{dropped} record file deleted"
            );
            assert!(
                manager.load_workspace_file(dropped).unwrap().is_none(),
                "{dropped} workspace file deleted"
            );
        }
        // The target keeps both halves.
        assert!(manager.load_record_file("rec-target").unwrap().is_some());
        assert!(manager.load_workspace_file("rec-target").unwrap().is_some());
    }

    /// `target = None` empties the name: every record filed under it is dropped,
    /// and records for OTHER names are untouched.
    #[test]
    fn restore_to_snapshot_none_empties_only_the_named_thread() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());

        let mut a1 = sample_thread();
        a1.id = "a1".to_string();
        a1.thread = "feature/a".to_string();
        manager.save(&a1).unwrap();
        let mut a2 = sample_thread();
        a2.id = "a2".to_string();
        a2.thread = "feature/a".to_string();
        a2.updated_at = a1.updated_at + chrono::Duration::seconds(5);
        manager.save(&a2).unwrap();
        let mut other = sample_thread();
        other.id = "b1".to_string();
        other.thread = "feature/b".to_string();
        manager.save(&other).unwrap();

        manager.restore_to_snapshot("feature/a", None).unwrap();

        assert!(
            manager.find_by_thread("feature/a").unwrap().is_none(),
            "all records for the named thread are deleted"
        );
        assert_eq!(
            manager.find_by_thread("feature/b").unwrap().unwrap().id,
            "b1",
            "records for other threads are untouched"
        );
    }

    /// When `target` is already the sole record under the name, the converge is a
    /// no-op that leaves it intact.
    #[test]
    fn restore_to_snapshot_target_already_sole_is_noop() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let name = "feature/sole";

        let mut rec = sample_thread();
        rec.id = "rec-sole".to_string();
        rec.thread = name.to_string();
        manager.save(&rec).unwrap();

        manager.restore_to_snapshot(name, Some(&rec)).unwrap();

        assert_eq!(
            manager.find_by_thread(name).unwrap().unwrap().id,
            "rec-sole"
        );
        assert_eq!(
            manager
                .list()
                .unwrap()
                .into_iter()
                .filter(|t| t.thread == name)
                .count(),
            1
        );
    }

    /// Pre-2.2 thread records have no `auto` field on disk. Reading
    /// them must succeed and surface `auto = false` so the default
    /// list view shows them (since we have no positive evidence they
    /// were harness-created).
    ///
    /// We synthesise a "legacy" on-disk record by saving a normal one
    /// and then stripping the `auto` line from the TOML — that way
    /// the rest of the schema (including the hex-encoded filename and
    /// the canonical TOML grammar) tracks the live store.
    #[test]
    fn thread_manager_defaults_auto_false_for_legacy_record() {
        let temp = TempDir::new().unwrap();
        let manager = ThreadManager::new(temp.path());
        let mut thread = sample_thread();
        thread.id = "legacy-thread".to_string();
        thread.thread = "legacy/branch".to_string();
        thread.auto = true; // make sure stripping has an observable effect
        manager.save(&thread).unwrap();

        // Find the record file and remove the `auto = true` line so
        // the loader sees a pre-2.2 schema.
        let records_dir = temp.path().join("thread_records");
        let entry = std::fs::read_dir(&records_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.path()
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x == "toml")
                    .unwrap_or(false)
            })
            .expect("at least one record file");
        let path = entry.path();
        let content = std::fs::read_to_string(&path).unwrap();
        let stripped: String = content
            .lines()
            .filter(|line| !line.trim_start().starts_with("auto"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, stripped).unwrap();

        let record = manager
            .load_record(&thread.id)
            .unwrap()
            .expect("record loads");
        assert!(
            !record.auto,
            "legacy records (no `auto` key on disk) must deserialize as auto=false"
        );
    }
}
