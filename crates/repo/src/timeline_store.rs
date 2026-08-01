// SPDX-License-Identifier: Apache-2.0
//! Filesystem store for agent timeline operation objects.

use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::RwLock,
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::{RepoLock, WriteLockGuard},
    object::{
        StateId, TimelineBranchId, TimelineCodecError, TimelineCursorMoveReason,
        TimelineOperationEnvelope, TimelineOperationId, TimelineStepId,
    },
    store::recover_pack_install_intents,
};
use serde::{Deserialize, Serialize};

use crate::{thread_manifest::encode_thread_segment, timeline_pack::TimelinePackSet};

pub const TIMELINE_MATERIALIZATION_RECOVERY_SCHEMA_VERSION: u16 = 1;
pub const TIMELINE_OPERATION_INDEX_SCHEMA_VERSION: u16 = 1;
const TIMELINE_DIR: &str = "timeline";
const OPS_DIR: &str = "ops";
const PACKS_DIR: &str = "packs";
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
    packs: RwLock<TimelinePackSet>,
}

impl TimelineStore {
    /// Open or create the timeline store under `<heddle_dir>/timeline`.
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        let root = heddle_dir.as_ref().join(TIMELINE_DIR);
        fs::create_dir_all(root.join(PACKS_DIR))?;
        recover_pack_install_intents(&root.join(PACKS_DIR))?;
        let store = Self {
            lock: RepoLock::at(root.join(LOCK_FILE)),
            packs: RwLock::new(TimelinePackSet::open(root.join(PACKS_DIR))?),
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
        fs::create_dir_all(self.packs_dir())?;
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let mut packs = self.packs.write().map_err(timeline_pack_lock_error)?;
                packs.reload_if_disk_changed()?;
                packs.read_operation(id)
            }
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

    pub(crate) fn canonical_operation_ids(&self) -> Result<Vec<TimelineOperationId>> {
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        let mut operation_ids = self
            .loose_operation_paths()?
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let mut packs = self.packs.write().map_err(timeline_pack_lock_error)?;
        packs.reload_if_disk_changed()?;
        operation_ids.extend(packs.operation_ids());
        operation_ids.sort();
        operation_ids.dedup();
        Ok(operation_ids)
    }

    #[cfg(test)]
    pub(crate) fn loose_operation_ids(&self) -> Result<Vec<TimelineOperationId>> {
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        Ok(self
            .loose_operation_paths()?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    /// Number of loose canonical timeline-operation files.
    pub fn loose_operation_count(&self) -> Result<u64> {
        let _guard = self.lock.read().map_err(timeline_lock_error)?;
        u64::try_from(self.loose_operation_paths()?.len()).map_err(|_| {
            HeddleError::InvalidObject("timeline operation count exceeds u64".to_string())
        })
    }

    /// Consolidate every canonical timeline operation into one pack.
    pub fn pack_operations(&self, aggressive: bool) -> Result<(u64, u64)> {
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        let loose_operations = self.read_loose_operations()?;
        let mut packs = self.packs.write().map_err(timeline_pack_lock_error)?;
        packs.consolidate(loose_operations, aggressive)
    }

    /// Remove loose operations only when an identical packed copy resolves.
    pub fn prune_loose_operations(&self) -> Result<(u64, u64)> {
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        let loose_paths = self.loose_operation_paths()?;
        let mut packs = self.packs.write().map_err(timeline_pack_lock_error)?;
        packs.reload()?;
        let mut removed = 0u64;
        let mut bytes_freed = 0u64;
        for (id, path) in loose_paths {
            let loose_bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };
            if packs.read_operation(&id)?.as_deref() != Some(loose_bytes.as_slice()) {
                continue;
            }
            let len = fs::metadata(&path)?.len();
            match fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    bytes_freed += len;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        remove_empty_operation_shards(&self.ops_dir())?;
        Ok((removed, bytes_freed))
    }

    /// Remove timeline `.pack` files that have no matching index.
    pub fn prune_unpaired_packs(&self) -> Result<(u64, u64)> {
        let _guard = self.lock.write().map_err(timeline_lock_error)?;
        let mut removed = 0u64;
        let mut bytes_freed = 0u64;
        for entry in fs::read_dir(self.packs_dir())? {
            let path = entry?.path();
            if !path.is_file()
                || !path
                    .extension()
                    .is_some_and(|extension| extension == "pack")
                || path.with_extension("idx").is_file()
            {
                continue;
            }
            let len = fs::metadata(&path)?.len();
            match fs::remove_file(path) {
                Ok(()) => {
                    removed += 1;
                    bytes_freed += len;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok((removed, bytes_freed))
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

    fn loose_operation_paths(&self) -> Result<Vec<(TimelineOperationId, PathBuf)>> {
        let mut paths = Vec::new();
        collect_loose_operation_paths(&self.ops_dir(), &mut paths)?;
        paths.sort_by_key(|(id, _)| *id);
        Ok(paths)
    }

    fn read_loose_operations(&self) -> Result<Vec<(TimelineOperationId, Vec<u8>)>> {
        self.loose_operation_paths()?
            .into_iter()
            .map(|(id, path)| {
                let bytes = fs::read(path)?;
                let computed_id = TimelineOperationId::for_bytes(&bytes);
                if computed_id != id {
                    return Err(HeddleError::InvalidObject(format!(
                        "loose timeline operation id mismatch: expected {}, decoded {}",
                        id.short(),
                        computed_id.short()
                    )));
                }
                TimelineOperationEnvelope::decode(&bytes).map_err(timeline_codec_error)?;
                Ok((id, bytes))
            })
            .collect()
    }

    fn packs_dir(&self) -> PathBuf {
        self.root.join(PACKS_DIR)
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

fn collect_loose_operation_paths(
    dir: &Path,
    paths: &mut Vec<(TimelineOperationId, PathBuf)>,
) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_loose_operation_paths(&path, paths)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "msgpack")
        {
            paths.push((operation_id_from_path(&path)?, path));
        }
    }
    Ok(())
}

fn operation_id_from_path(path: &Path) -> Result<TimelineOperationId> {
    let prefix = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "timeline operation path has no shard prefix: {}",
                path.display()
            ))
        })?;
    let rest = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            HeddleError::InvalidObject(format!(
                "timeline operation path has no file stem: {}",
                path.display()
            ))
        })?;
    let raw = hex::decode(format!("{prefix}{rest}")).map_err(|err| {
        HeddleError::InvalidObject(format!(
            "timeline operation path has invalid id '{}': {err}",
            path.display()
        ))
    })?;
    TimelineOperationId::try_from_slice(&raw).map_err(|err| {
        HeddleError::InvalidObject(format!(
            "timeline operation path has invalid id length '{}': {err}",
            path.display()
        ))
    })
}

fn remove_empty_operation_shards(ops_dir: &Path) -> Result<()> {
    for entry in fs::read_dir(ops_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if fs::read_dir(&path)?.next().is_none() {
            match fs::remove_dir(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
    }
    Ok(())
}

fn timeline_codec_error(err: TimelineCodecError) -> HeddleError {
    HeddleError::InvalidObject(err.to_string())
}

fn timeline_lock_error(err: objects::lock::LockError) -> HeddleError {
    HeddleError::InvalidObject(format!("acquire timeline store lock: {err}"))
}

fn timeline_pack_lock_error<T>(_: std::sync::PoisonError<T>) -> HeddleError {
    HeddleError::InvalidObject("acquire timeline pack index lock".to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

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

    fn directory_sizes(path: &Path) -> (u64, u64) {
        let metadata = std::fs::metadata(path).unwrap();
        let mut apparent = metadata.len();
        #[cfg(unix)]
        let mut allocated = metadata.blocks() * 512;
        #[cfg(not(unix))]
        let mut allocated = apparent;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                let (entry_apparent, entry_allocated) = directory_sizes(&entry.unwrap().path());
                apparent += entry_apparent;
                allocated += entry_allocated;
            }
        }
        (apparent, allocated)
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
    fn timeline_store_fails_open_on_corrupt_paired_pack() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let store = TimelineStore::open(&heddle_dir).unwrap();
        std::fs::write(store.packs_dir().join("corrupt.pack"), b"not a pack").unwrap();
        std::fs::write(store.packs_dir().join("corrupt.idx"), b"not an index").unwrap();
        drop(store);

        assert!(TimelineStore::open(&heddle_dir).is_err());
    }

    #[test]
    fn timeline_store_prunes_only_unpaired_pack_files() {
        let temp = TempDir::new().unwrap();
        let store = TimelineStore::open(temp.path().join(".heddle")).unwrap();
        std::fs::write(store.packs_dir().join("orphan.pack"), b"orphan").unwrap();
        std::fs::write(store.packs_dir().join("index-only.idx"), b"index").unwrap();

        assert_eq!(store.prune_unpaired_packs().unwrap(), (1, 6));
        assert!(!store.packs_dir().join("orphan.pack").exists());
        assert!(store.packs_dir().join("index-only.idx").exists());
    }

    #[test]
    fn timeline_repack_carries_forward_existing_packed_operations() {
        let temp = TempDir::new().unwrap();
        let store = TimelineStore::open(temp.path().join(".heddle")).unwrap();
        let first = sample_envelope();
        let first_id = store.write_operation(&first).unwrap();
        let first_bytes = first.encode().unwrap();
        store.pack_operations(false).unwrap();
        store.prune_loose_operations().unwrap();

        let second = TimelineOperationEnvelope::new(
            TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                thread: "main".to_string(),
                branch_id: TimelineBranchId::new("tlb-second"),
                parent_branch_id: Some(TimelineBranchId::new("tlb-child")),
                from_step_id: Some(TimelineStepId::new("tls-root")),
                from_state: StateId::from_bytes([1; 32]),
                reason: TimelineBranchReason::FanOut,
                created_at_ms: 1_700_000_000_001,
            }),
            Vec::new(),
        );
        let second_id = store.write_operation(&second).unwrap();
        let second_bytes = second.encode().unwrap();

        assert_eq!(store.pack_operations(true).unwrap().0, 2);
        assert_eq!(store.prune_loose_operations().unwrap().0, 1);
        assert_eq!(
            store.read_operation_bytes(&first_id).unwrap(),
            Some(first_bytes)
        );
        assert_eq!(
            store.read_operation_bytes(&second_id).unwrap(),
            Some(second_bytes)
        );
    }

    #[test]
    fn packing_1000_branches_preserves_ids_bytes_and_removes_block_slack() {
        let temp = TempDir::new().unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let store = TimelineStore::open(&heddle_dir).unwrap();
        let mut expected = BTreeMap::new();
        for ordinal in 0..1_000 {
            let envelope = TimelineOperationEnvelope::new(
                TimelineOperationBodyV1::BranchCreated(BranchCreatedV1 {
                    thread: "main".to_string(),
                    branch_id: TimelineBranchId::new(format!("tlb-{ordinal:04}")),
                    parent_branch_id: None,
                    from_step_id: None,
                    from_state: StateId::from_bytes([1; 32]),
                    reason: TimelineBranchReason::FanOut,
                    created_at_ms: ordinal,
                }),
                Vec::new(),
            );
            let bytes = envelope.encode().unwrap();
            let id = TimelineOperationId::for_bytes(&bytes);
            let path = store.operation_path(&id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &bytes).unwrap();
            expected.insert(id, bytes);
        }
        store
            .write_operation_index(&expected.keys().copied().collect::<Vec<_>>())
            .unwrap();
        let (before_apparent, before_allocated) = directory_sizes(store.root());

        let (packed, _) = store.pack_operations(false).unwrap();
        assert_eq!(packed, 1_000);
        let (pruned, _) = store.prune_loose_operations().unwrap();
        assert_eq!(pruned, 1_000);
        let (after_apparent, after_allocated) = directory_sizes(store.root());
        drop(store);
        let store = TimelineStore::open(&heddle_dir).unwrap();
        assert_eq!(
            store.canonical_operation_ids().unwrap(),
            expected.keys().copied().collect::<Vec<_>>()
        );

        for (id, expected_bytes) in &expected {
            assert!(!store.operation_path(id).exists());
            let packed_bytes = store.read_operation_bytes(id).unwrap().unwrap();
            assert_eq!(&packed_bytes, expected_bytes);
            assert_eq!(TimelineOperationId::for_bytes(&packed_bytes), *id);
        }
        assert_eq!(
            crate::TimelineView::rebuild(&store)
                .unwrap()
                .branch_count("main"),
            1_000
        );
        assert_eq!(store.pack_operations(false).unwrap(), (0, 0));
        assert_eq!(store.prune_loose_operations().unwrap(), (0, 0));
        println!(
            "1000-branch canonical store: apparent {before_apparent} -> {after_apparent} bytes; allocated {before_allocated} -> {after_allocated} bytes"
        );
        #[cfg(unix)]
        assert!(after_allocated < before_allocated / 2);
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
