// SPDX-License-Identifier: Apache-2.0
//! Filesystem store for agent timeline operation objects.

use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::{RepoLock, WriteLockGuard},
    object::{
        StateId, TimelineBranchId, TimelineCodecError, TimelineCursorMoveReason,
        TimelineOperationEnvelope, TimelineOperationId, TimelineStepId,
    },
};
use serde::{Deserialize, Serialize};

use crate::thread_manifest::encode_thread_segment;

pub const TIMELINE_MATERIALIZATION_RECOVERY_SCHEMA_VERSION: u16 = 1;
pub const TIMELINE_OPERATION_INDEX_SCHEMA_VERSION: u16 = 1;
const TIMELINE_DIR: &str = "timeline";
const OPS_DIR: &str = "ops";
const INDEXES_DIR: &str = "indexes";
const VIEWS_DIR: &str = "views";
const SYNC_DIR: &str = "sync";
const RECOVERY_DIR: &str = "recovery";
const LOCKS_DIR: &str = "locks";
const TMP_DIR: &str = "tmp";
const LOCK_FILE: &str = "timeline.lock";
const OPERATION_INDEX_FILE: &str = "operations.msgpack";
const VIEW_CHECKPOINT_FILE: &str = "timeline-view.msgpack";
const MATERIALIZATION_RECOVERY_EXT: &str = "materialization.msgpack";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TimelineOperationIndex {
    schema_version: u16,
    operation_ids: Vec<TimelineOperationId>,
}

/// Versioned sidecar used to complete a timeline cursor move after crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineMaterializationRecoveryRecord {
    pub schema_version: u16,
    pub thread: String,
    pub branch_id: TimelineBranchId,
    pub from_step_id: Option<TimelineStepId>,
    pub to_step_id: Option<TimelineStepId>,
    pub from_state: StateId,
    pub to_state: StateId,
    pub reason: TimelineCursorMoveReason,
    pub moved_at_ms: i64,
}

impl TimelineMaterializationRecoveryRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        thread: impl Into<String>,
        branch_id: TimelineBranchId,
        from_step_id: Option<TimelineStepId>,
        to_step_id: Option<TimelineStepId>,
        from_state: StateId,
        to_state: StateId,
        reason: TimelineCursorMoveReason,
        moved_at_ms: i64,
    ) -> Self {
        Self {
            schema_version: TIMELINE_MATERIALIZATION_RECOVERY_SCHEMA_VERSION,
            thread: thread.into(),
            branch_id,
            from_step_id,
            to_step_id,
            from_state,
            to_state,
            reason,
            moved_at_ms,
        }
    }
}

/// Durable local store for content-addressed timeline operations.
pub struct TimelineStore {
    root: PathBuf,
    lock: RepoLock,
}

impl TimelineStore {
    /// Open or create the timeline store under `<heddle_dir>/timeline`.
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        let root = heddle_dir.as_ref().join(TIMELINE_DIR);
        let store = Self {
            lock: RepoLock::at(root.join(LOCK_FILE)),
            root,
        };
        store.init()?;
        Ok(store)
    }

    /// Store root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Ensure the timeline layout exists.
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(self.ops_dir())?;
        fs::create_dir_all(self.root.join(INDEXES_DIR))?;
        fs::create_dir_all(self.root.join(VIEWS_DIR))?;
        fs::create_dir_all(self.root.join(SYNC_DIR))?;
        fs::create_dir_all(self.root.join(RECOVERY_DIR))?;
        fs::create_dir_all(self.root.join(LOCKS_DIR))?;
        fs::create_dir_all(self.root.join(TMP_DIR))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.lock_path())?;
        Ok(())
    }

    /// Write an operation envelope and return its content-addressed id.
    pub fn write_operation(
        &self,
        envelope: &TimelineOperationEnvelope,
    ) -> Result<TimelineOperationId> {
        let bytes = envelope.encode().map_err(timeline_codec_error)?;
        self.write_operation_bytes(&bytes)
    }

    /// Write canonical operation envelope bytes and return their id.
    pub fn write_operation_bytes(&self, bytes: &[u8]) -> Result<TimelineOperationId> {
        TimelineOperationEnvelope::decode(bytes).map_err(timeline_codec_error)?;
        let id = TimelineOperationId::for_bytes(bytes);
        let path = self.operation_path(&id);
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_file_atomic(&path, bytes)?;
        }
        let index_path = self.operation_index_path();
        let mut operation_ids = read_operation_index_unlocked(&index_path)
            .unwrap_or(None)
            .unwrap_or_default();
        if !operation_ids.contains(&id) {
            operation_ids.push(id);
            write_operation_index_unlocked(&index_path, &operation_ids)?;
        }
        Ok(id)
    }

    /// Read canonical operation envelope bytes by id.
    pub fn read_operation_bytes(&self, id: &TimelineOperationId) -> Result<Option<Vec<u8>>> {
        let path = self.operation_path(id);
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Read and decode an operation envelope by id.
    pub fn read_operation(
        &self,
        id: &TimelineOperationId,
    ) -> Result<Option<TimelineOperationEnvelope>> {
        let Some(bytes) = self.read_operation_bytes(id)? else {
            return Ok(None);
        };
        TimelineOperationEnvelope::decode(&bytes)
            .map(Some)
            .map_err(timeline_codec_error)
    }

    /// Sharded path for an operation id.
    pub fn operation_path(&self, id: &TimelineOperationId) -> PathBuf {
        let hex = id.to_hex();
        let (prefix, rest) = hex.split_at(2);
        self.ops_dir().join(prefix).join(format!("{rest}.msgpack"))
    }

    pub(crate) fn read_operation_index(&self) -> Result<Option<Vec<TimelineOperationId>>> {
        let path = self.operation_index_path();
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        read_operation_index_unlocked(&path)
    }

    pub(crate) fn write_operation_index(
        &self,
        operation_ids: &[TimelineOperationId],
    ) -> Result<()> {
        let path = self.operation_index_path();
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        write_operation_index_unlocked(&path, operation_ids)
    }

    pub(crate) fn read_view_checkpoint_bytes(&self) -> Result<Option<Vec<u8>>> {
        let path = self.view_checkpoint_path();
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub(crate) fn write_view_checkpoint_bytes(&self, bytes: &[u8]) -> Result<()> {
        let path = self.view_checkpoint_path();
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, bytes)?;
        Ok(())
    }

    pub fn stage_materialization_recovery(
        &self,
        record: &TimelineMaterializationRecoveryRecord,
    ) -> Result<()> {
        let path = self.materialization_recovery_path(&record.thread);
        let bytes = rmp_serde::to_vec_named(record)
            .map_err(|err| HeddleError::Serialization(err.to_string()))?;
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    pub fn read_materialization_recovery(
        &self,
        thread: &str,
    ) -> Result<Option<TimelineMaterializationRecoveryRecord>> {
        let path = self.materialization_recovery_path(thread);
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let record: TimelineMaterializationRecoveryRecord = rmp_serde::from_slice(&bytes)
            .map_err(|err| HeddleError::InvalidObject(err.to_string()))?;
        if record.schema_version != TIMELINE_MATERIALIZATION_RECOVERY_SCHEMA_VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported timeline materialization recovery schema version {}",
                record.schema_version
            )));
        }
        if record.thread != thread {
            return Err(HeddleError::InvalidObject(format!(
                "timeline materialization recovery thread mismatch: expected '{thread}', found '{}'",
                record.thread
            )));
        }
        Ok(Some(record))
    }

    pub fn clear_materialization_recovery(&self, thread: &str) -> Result<()> {
        let path = self.materialization_recovery_path(thread);
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn materialization_recovery_path(&self, thread: &str) -> PathBuf {
        self.root.join(RECOVERY_DIR).join(format!(
            "{}.{MATERIALIZATION_RECOVERY_EXT}",
            encode_thread_segment(thread)
        ))
    }

    pub fn lock_materialization(&self, thread: &str) -> Result<WriteLockGuard> {
        RepoLock::at(self.materialization_lock_path(thread))
            .write()
            .map_err(timeline_lock_error)
    }

    pub fn materialization_lock_path(&self, thread: &str) -> PathBuf {
        self.root.join(LOCKS_DIR).join(format!(
            "{}.materialization.lock",
            encode_thread_segment(thread)
        ))
    }

    pub fn lock_recording(&self, thread: &str) -> Result<WriteLockGuard> {
        RepoLock::at(self.recording_lock_path(thread))
            .write()
            .map_err(timeline_lock_error)
    }

    pub fn recording_lock_path(&self, thread: &str) -> PathBuf {
        self.root
            .join(LOCKS_DIR)
            .join(format!("{}.recording.lock", encode_thread_segment(thread)))
    }

    fn ops_dir(&self) -> PathBuf {
        self.root.join(OPS_DIR)
    }

    fn operation_index_path(&self) -> PathBuf {
        self.root.join(INDEXES_DIR).join(OPERATION_INDEX_FILE)
    }

    fn view_checkpoint_path(&self) -> PathBuf {
        self.root.join(VIEWS_DIR).join(VIEW_CHECKPOINT_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE)
    }
}

fn read_operation_index_unlocked(path: &Path) -> Result<Option<Vec<TimelineOperationId>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let index: TimelineOperationIndex =
        rmp_serde::from_slice(&bytes).map_err(|err| HeddleError::InvalidObject(err.to_string()))?;
    if index.schema_version != TIMELINE_OPERATION_INDEX_SCHEMA_VERSION {
        return Err(HeddleError::InvalidObject(format!(
            "unsupported timeline operation index schema version {}",
            index.schema_version
        )));
    }
    Ok(Some(index.operation_ids))
}

fn write_operation_index_unlocked(
    path: &Path,
    operation_ids: &[TimelineOperationId],
) -> Result<()> {
    let index = TimelineOperationIndex {
        schema_version: TIMELINE_OPERATION_INDEX_SCHEMA_VERSION,
        operation_ids: operation_ids.to_vec(),
    };
    let bytes = rmp_serde::to_vec_named(&index)
        .map_err(|err| HeddleError::Serialization(err.to_string()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_file_atomic(path, &bytes)?;
    Ok(())
}

fn timeline_codec_error(err: TimelineCodecError) -> HeddleError {
    HeddleError::InvalidObject(err.to_string())
}

fn timeline_lock_error(err: objects::lock::LockError) -> HeddleError {
    HeddleError::InvalidObject(format!("acquire timeline store lock: {err}"))
}

#[cfg(test)]
mod tests {
    use objects::object::{
        BranchCreatedV1, StateId, TimelineBranchId, TimelineBranchReason, TimelineOperationBodyV1,
        TimelineOperationEnvelope, TimelineStepId,
    };
    use tempfile::TempDir;

    use super::*;

    fn sample_envelope() -> TimelineOperationEnvelope {
        TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                thread: "main".to_string(),
                branch_id: TimelineBranchId::new("tlb-child"),
                parent_branch_id: Some(TimelineBranchId::new("tlb-main")),
                from_step_id: Some(TimelineStepId::new("tls-root")),
                from_state: StateId::from_bytes([1; 32]),
                reason: TimelineBranchReason::ExplicitFork,
                created_at_ms: 1_700_000_000_000,
            }),
            Vec::new(),
        )
    }

    #[test]
    fn timeline_store_writes_op_and_reads_it_back() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let store = TimelineStore::open(&heddle_dir).unwrap();

        let envelope = sample_envelope();
        let id = store.write_operation(&envelope).unwrap();

        assert!(store.root().join("ops").is_dir());
        assert!(store.root().join("indexes").is_dir());
        assert!(store.root().join("views").is_dir());
        assert!(store.root().join("sync").is_dir());
        assert!(store.root().join("recovery").is_dir());
        assert!(store.root().join("locks").is_dir());
        assert!(store.root().join("tmp").is_dir());
        assert!(store.root().join("timeline.lock").is_file());
        assert!(store.operation_path(&id).is_file());

        let read = store.read_operation(&id).unwrap().unwrap();
        assert_eq!(read, envelope);
        assert_eq!(
            store.read_operation_bytes(&id).unwrap().unwrap(),
            envelope.encode().unwrap()
        );
    }

    #[test]
    fn timeline_store_repairs_corrupt_operation_index_on_write() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let store = TimelineStore::open(&heddle_dir).unwrap();
        let id = store.write_operation(&sample_envelope()).unwrap();
        std::fs::write(store.operation_index_path(), b"not msgpack").unwrap();

        assert_eq!(store.write_operation(&sample_envelope()).unwrap(), id);
        assert_eq!(store.read_operation_index().unwrap(), Some(vec![id]));
    }

    #[test]
    fn timeline_store_round_trips_materialization_recovery_record() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let store = TimelineStore::open(&heddle_dir).unwrap();
        let record = TimelineMaterializationRecoveryRecord::new(
            "feature/slashed",
            TimelineBranchId::new("tlb-main"),
            Some(TimelineStepId::new("tls-before")),
            Some(TimelineStepId::new("tls-after")),
            StateId::from_bytes([1; 32]),
            StateId::from_bytes([2; 32]),
            TimelineCursorMoveReason::SeekToolCall,
            42,
        );

        store.stage_materialization_recovery(&record).unwrap();

        assert!(
            store
                .materialization_recovery_path("feature/slashed")
                .is_file()
        );
        assert_eq!(
            store
                .read_materialization_recovery("feature/slashed")
                .unwrap(),
            Some(record)
        );

        store
            .clear_materialization_recovery("feature/slashed")
            .unwrap();
        assert!(
            store
                .read_materialization_recovery("feature/slashed")
                .unwrap()
                .is_none()
        );
    }
}
