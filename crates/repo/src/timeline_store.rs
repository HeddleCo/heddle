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
    lock::RepoLock,
    object::{TimelineCodecError, TimelineOperationEnvelope, TimelineOperationId},
};

const TIMELINE_DIR: &str = "timeline";
const OPS_DIR: &str = "ops";
const INDEXES_DIR: &str = "indexes";
const VIEWS_DIR: &str = "views";
const SYNC_DIR: &str = "sync";
const TMP_DIR: &str = "tmp";
const LOCK_FILE: &str = "timeline.lock";

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

    fn ops_dir(&self) -> PathBuf {
        self.root.join(OPS_DIR)
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE)
    }
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
        BranchCreatedV1, ChangeId, TimelineBranchId, TimelineBranchReason, TimelineOperationBodyV1,
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
                from_state: ChangeId::from_bytes([1; 16]),
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
}
