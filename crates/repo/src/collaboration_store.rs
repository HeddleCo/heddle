// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use objects::{
    error::{HeddleError, Result},
    fs_atomic::write_file_atomic,
    lock::RepoLock,
    object::{
        CollabOpId, CollaborationIdempotencyKey, CollaborationOperationEnvelope,
        DecodedCollaborationOperation, DiscussionRecordId, MaterializedRepositoryCollaboration,
        materialize_repository_collaboration,
    },
};
use serde::{Deserialize, Serialize};

const INDEX_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationWriteDisposition {
    Created,
    ExistingOperation,
    IdempotentReplay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationWriteOutcome {
    pub operation_id: CollabOpId,
    pub disposition: CollaborationWriteDisposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationIntegrityReport {
    pub operation_count: usize,
    pub discussion_count: usize,
    pub index_current: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoreIndex {
    version: u16,
    operations: BTreeSet<CollabOpId>,
    discussions: BTreeMap<DiscussionRecordId, BTreeSet<CollabOpId>>,
    idempotency: BTreeMap<IdempotencyScope, CollabOpId>,
}

impl Default for StoreIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            operations: BTreeSet::new(),
            discussions: BTreeMap::new(),
            idempotency: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct IdempotencyScope {
    discussion_id: DiscussionRecordId,
    operation_kind: String,
    principal_email: String,
    key: CollaborationIdempotencyKey,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct PendingCommit {
    operation_id: CollabOpId,
}

pub struct CollaborationStore {
    root: PathBuf,
    lock: RepoLock,
}

impl CollaborationStore {
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        let root = heddle_dir.as_ref().join("collaboration");
        fs::create_dir_all(root.join("ops"))?;
        fs::create_dir_all(root.join("indexes"))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("store.lock"))?;
        let store = Self {
            lock: RepoLock::at(root.join("store.lock")),
            root,
        };
        let _guard = store.lock.write().map_err(lock_error)?;
        if store.pending_commit_path().exists() {
            let index = store.rebuild_index_unlocked()?;
            store.write_index_unlocked(&index)?;
            store.clear_pending_commit_unlocked()?;
        } else {
            match store.read_index_unlocked() {
                Ok(Some(_)) => {}
                Ok(None) | Err(HeddleError::InvalidObject(_)) => {
                    let index = store.rebuild_index_unlocked()?;
                    store.write_index_unlocked(&index)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_operation(
        &self,
        operation: &CollaborationOperationEnvelope,
    ) -> Result<CollaborationWriteOutcome> {
        let bytes = operation.encode().map_err(codec_error)?;
        self.write_operation_bytes(&bytes)
    }

    pub fn write_operation_bytes(&self, bytes: &[u8]) -> Result<CollaborationWriteOutcome> {
        let decoded = CollaborationOperationEnvelope::decode(bytes).map_err(codec_error)?;
        let id = decoded.operation_id;
        let scope = idempotency_scope(&decoded.operation);
        let _guard = self.lock.write().map_err(lock_error)?;
        if self.pending_commit_path().exists() {
            let recovered = self.rebuild_index_unlocked()?;
            self.write_index_unlocked(&recovered)?;
            self.clear_pending_commit_unlocked()?;
        }
        let mut index = self.read_or_rebuild_index_unlocked()?;
        if let Some(existing) = index.idempotency.get(&scope) {
            if *existing != id {
                return Err(HeddleError::InvalidObject(format!(
                    "collaboration idempotency key already identifies {existing}"
                )));
            }
            return Ok(CollaborationWriteOutcome {
                operation_id: id,
                disposition: CollaborationWriteDisposition::IdempotentReplay,
            });
        }

        let path = self.operation_path(&id);
        let existed = match fs::read(&path) {
            Ok(existing) if existing == bytes => true,
            Ok(_) => {
                return Err(HeddleError::InvalidObject(format!(
                    "collaboration operation address collision at {id}"
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        self.stage_pending_commit_unlocked(id)?;
        if !existed {
            fs::create_dir_all(path.parent().expect("operation path has shard"))?;
            write_file_atomic(&path, bytes)?;
        }
        index.operations.insert(id);
        index
            .discussions
            .entry(decoded.operation.discussion_id)
            .or_default()
            .insert(id);
        index.idempotency.insert(scope, id);
        self.write_index_unlocked(&index)?;
        self.clear_pending_commit_unlocked()?;
        Ok(CollaborationWriteOutcome {
            operation_id: id,
            disposition: if existed {
                CollaborationWriteDisposition::ExistingOperation
            } else {
                CollaborationWriteDisposition::Created
            },
        })
    }

    /// Retire `retire` and write `write` under one lock.
    ///
    /// Hosted discussion sync uses this to replace an originator's local-only
    /// turn ops with the hosted-canonical envelopes a clone would materialize
    /// (heddle#1695). New ids are new envelopes; this does not rewrite bytes
    /// under an existing `CollabOpId`.
    ///
    /// Retire-then-write crash recovery is the existing pending-commit rebuild
    /// (`stage_pending_commit_unlocked` → `pending-commit.msgpack` →
    /// `rebuild_index_unlocked` on next `open`).
    pub fn replace_operations(
        &self,
        retire: &[CollabOpId],
        write: &[CollaborationOperationEnvelope],
    ) -> Result<Vec<CollaborationWriteOutcome>> {
        if retire.is_empty() && write.is_empty() {
            return Ok(Vec::new());
        }
        let decoded_writes = write
            .iter()
            .map(|operation| {
                let bytes = operation.encode().map_err(codec_error)?;
                let decoded =
                    CollaborationOperationEnvelope::decode(&bytes).map_err(codec_error)?;
                Ok((bytes, decoded))
            })
            .collect::<Result<Vec<_>>>()?;
        let _guard = self.lock.write().map_err(lock_error)?;
        if self.pending_commit_path().exists() {
            let recovered = self.rebuild_index_unlocked()?;
            self.write_index_unlocked(&recovered)?;
            self.clear_pending_commit_unlocked()?;
        }
        let mut index = self.read_or_rebuild_index_unlocked()?;
        let retire_set: BTreeSet<CollabOpId> = retire.iter().copied().collect();

        for (bytes, decoded) in &decoded_writes {
            let scope = idempotency_scope(&decoded.operation);
            if let Some(existing) = index.idempotency.get(&scope)
                && !retire_set.contains(existing)
                && *existing != decoded.operation_id
            {
                return Err(HeddleError::InvalidObject(format!(
                    "collaboration idempotency key already identifies {existing}"
                )));
            }
            let path = self.operation_path(&decoded.operation_id);
            match fs::read(&path) {
                Ok(existing) if existing == *bytes => {}
                Ok(_) => {
                    return Err(HeddleError::InvalidObject(format!(
                        "collaboration operation address collision at {}",
                        decoded.operation_id
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }

        for id in &retire_set {
            self.stage_pending_commit_unlocked(*id)?;
            let path = self.operation_path(id);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            index.operations.remove(id);
            for discussion_ops in index.discussions.values_mut() {
                discussion_ops.remove(id);
            }
            index
                .discussions
                .retain(|_, discussion_ops| !discussion_ops.is_empty());
            index.idempotency.retain(|_, existing| existing != id);
        }

        let mut outcomes = Vec::with_capacity(decoded_writes.len());
        for (bytes, decoded) in decoded_writes {
            let id = decoded.operation_id;
            let scope = idempotency_scope(&decoded.operation);
            let path = self.operation_path(&id);
            let existed = path.exists();
            self.stage_pending_commit_unlocked(id)?;
            if !existed {
                let parent = path.parent().ok_or_else(|| {
                    HeddleError::InvalidObject(format!(
                        "collaboration operation path has no shard directory: {}",
                        path.display()
                    ))
                })?;
                fs::create_dir_all(parent)?;
                write_file_atomic(&path, &bytes)?;
            }
            index.operations.insert(id);
            index
                .discussions
                .entry(decoded.operation.discussion_id)
                .or_default()
                .insert(id);
            index.idempotency.insert(scope, id);
            outcomes.push(CollaborationWriteOutcome {
                operation_id: id,
                disposition: if existed {
                    CollaborationWriteDisposition::ExistingOperation
                } else {
                    CollaborationWriteDisposition::Created
                },
            });
        }
        self.write_index_unlocked(&index)?;
        self.clear_pending_commit_unlocked()?;
        Ok(outcomes)
    }

    pub fn read_operation(&self, id: &CollabOpId) -> Result<Option<DecodedCollaborationOperation>> {
        let _guard = self.lock.read().map_err(lock_error)?;
        self.read_operation_unlocked(id)
    }

    pub fn operation_ids(&self) -> Result<Vec<CollabOpId>> {
        let _guard = self.lock.read().map_err(lock_error)?;
        Ok(self
            .read_or_rebuild_index_readonly_unlocked()?
            .operations
            .into_iter()
            .collect())
    }

    pub fn discussion_operation_ids(
        &self,
        discussion_id: &DiscussionRecordId,
    ) -> Result<Vec<CollabOpId>> {
        let _guard = self.lock.read().map_err(lock_error)?;
        Ok(self
            .read_or_rebuild_index_readonly_unlocked()?
            .discussions
            .remove(discussion_id)
            .unwrap_or_default()
            .into_iter()
            .collect())
    }

    pub fn materialize(&self) -> Result<MaterializedRepositoryCollaboration> {
        let operations = self
            .operation_ids()?
            .into_iter()
            .map(|id| {
                self.read_operation(&id)?.ok_or_else(|| {
                    HeddleError::InvalidObject(format!(
                        "collaboration index references missing operation {id}"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        materialize_repository_collaboration(operations).map_err(codec_error)
    }

    pub fn materialize_discussion(
        &self,
        discussion_id: &DiscussionRecordId,
    ) -> Result<Option<objects::object::MaterializedDiscussion>> {
        let operations = self
            .discussion_operation_ids(discussion_id)?
            .into_iter()
            .map(|id| {
                self.read_operation(&id)?.ok_or_else(|| {
                    HeddleError::InvalidObject(format!("missing collaboration operation {id}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(materialize_repository_collaboration(operations)
            .map_err(codec_error)?
            .discussions
            .remove(discussion_id))
    }

    pub fn rebuild_index(&self) -> Result<()> {
        let _guard = self.lock.write().map_err(lock_error)?;
        let index = self.rebuild_index_unlocked()?;
        self.write_index_unlocked(&index)
    }

    pub fn verify_integrity(&self) -> Result<CollaborationIntegrityReport> {
        let _guard = self.lock.read().map_err(lock_error)?;
        let rebuilt = self.rebuild_index_unlocked()?;
        let current = self.read_index_unlocked()?;
        Ok(CollaborationIntegrityReport {
            operation_count: rebuilt.operations.len(),
            discussion_count: rebuilt.discussions.len(),
            index_current: current.as_ref() == Some(&rebuilt),
        })
    }

    pub fn remove_all(&self) -> Result<()> {
        let _guard = self.lock.write().map_err(lock_error)?;
        match fs::remove_dir_all(self.root.join("ops")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(self.root.join("ops"))?;
        self.write_index_unlocked(&StoreIndex::default())
    }

    fn operation_path(&self, id: &CollabOpId) -> PathBuf {
        let hex = id.to_hex();
        self.root
            .join("ops")
            .join(&hex[..2])
            .join(format!("{}.msgpack", &hex[2..]))
    }

    fn read_operation_unlocked(
        &self,
        id: &CollabOpId,
    ) -> Result<Option<DecodedCollaborationOperation>> {
        let bytes = match fs::read(self.operation_path(id)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let decoded = CollaborationOperationEnvelope::decode(&bytes).map_err(codec_error)?;
        if decoded.operation_id != *id {
            return Err(HeddleError::InvalidObject(format!(
                "collaboration operation path {id} contains {}",
                decoded.operation_id
            )));
        }
        Ok(Some(decoded))
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("indexes").join("operations.msgpack")
    }

    fn pending_commit_path(&self) -> PathBuf {
        self.root.join("pending-commit.msgpack")
    }

    fn stage_pending_commit_unlocked(&self, operation_id: CollabOpId) -> Result<()> {
        let bytes = rmp_serde::to_vec_named(&PendingCommit { operation_id })
            .map_err(|error| HeddleError::Serialization(error.to_string()))?;
        write_file_atomic(&self.pending_commit_path(), &bytes)?;
        Ok(())
    }

    fn clear_pending_commit_unlocked(&self) -> Result<()> {
        match fs::remove_file(self.pending_commit_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn read_index_unlocked(&self) -> Result<Option<StoreIndex>> {
        let bytes = match fs::read(self.index_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let index: StoreIndex = rmp_serde::from_slice(&bytes)
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
        if index.version != INDEX_VERSION {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported collaboration index version {}",
                index.version
            )));
        }
        Ok(Some(index))
    }

    fn write_index_unlocked(&self, index: &StoreIndex) -> Result<()> {
        let bytes = rmp_serde::to_vec_named(index)
            .map_err(|error| HeddleError::Serialization(error.to_string()))?;
        write_file_atomic(&self.index_path(), &bytes)?;
        Ok(())
    }

    fn read_or_rebuild_index_unlocked(&self) -> Result<StoreIndex> {
        match self.read_index_unlocked() {
            Ok(Some(index)) => Ok(index),
            Ok(None) | Err(HeddleError::InvalidObject(_)) => {
                let index = self.rebuild_index_unlocked()?;
                self.write_index_unlocked(&index)?;
                Ok(index)
            }
            Err(error) => Err(error),
        }
    }

    fn read_or_rebuild_index_readonly_unlocked(&self) -> Result<StoreIndex> {
        match self.read_index_unlocked() {
            Ok(Some(index)) => Ok(index),
            Ok(None) | Err(HeddleError::InvalidObject(_)) => self.rebuild_index_unlocked(),
            Err(error) => Err(error),
        }
    }

    fn rebuild_index_unlocked(&self) -> Result<StoreIndex> {
        let mut index = StoreIndex::default();
        for shard in fs::read_dir(self.root.join("ops"))? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry.path().extension().and_then(|value| value.to_str()) != Some("msgpack")
                {
                    continue;
                }
                let bytes = fs::read(entry.path())?;
                let decoded =
                    CollaborationOperationEnvelope::decode(&bytes).map_err(codec_error)?;
                if self.operation_path(&decoded.operation_id) != entry.path() {
                    return Err(HeddleError::InvalidObject(format!(
                        "collaboration operation is stored at a non-canonical path: {}",
                        entry.path().display()
                    )));
                }
                let scope = idempotency_scope(&decoded.operation);
                if let Some(existing) = index.idempotency.insert(scope, decoded.operation_id)
                    && existing != decoded.operation_id
                {
                    return Err(HeddleError::InvalidObject(
                        "collaboration idempotency collision while rebuilding index".to_string(),
                    ));
                }
                index.operations.insert(decoded.operation_id);
                index
                    .discussions
                    .entry(decoded.operation.discussion_id)
                    .or_default()
                    .insert(decoded.operation_id);
            }
        }
        Ok(index)
    }
}

fn idempotency_scope(operation: &CollaborationOperationEnvelope) -> IdempotencyScope {
    IdempotencyScope {
        discussion_id: operation.discussion_id,
        operation_kind: operation.body.kind_name().to_string(),
        principal_email: operation.author.principal.email_lossy().into_owned(),
        key: operation.idempotency_key.clone(),
    }
}

fn codec_error(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(error.to_string())
}

fn lock_error(error: objects::lock::LockError) -> HeddleError {
    error.into()
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, thread};

    use objects::object::{
        Attribution, CollaborationAnchor, CollaborationOperationBodyV1, DiscussionTurnV1,
        Principal, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;

    fn operation(key: &str) -> CollaborationOperationEnvelope {
        CollaborationOperationEnvelope::new(
            "disc-018f47ea-4a54-7c89-b012-3456789abcde".parse().unwrap(),
            Vec::new(),
            CollaborationIdempotencyKey::new(key).unwrap(),
            Attribution::human(Principal::new("Ada", "ada@example.com")),
            1,
            CollaborationOperationBodyV1::Open {
                title: format!("Review {key}"),
                anchor: CollaborationAnchor::Repository,
                visibility: VisibilityTier::default(),
                turn: DiscussionTurnV1::new("first").unwrap(),
                thread_ref: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn operation_file_is_commit_point_and_index_is_rebuildable() {
        let temp = TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let operation = operation("one");
        let bytes = operation.encode().unwrap();
        let operation_id = CollabOpId::for_bytes(&bytes);
        store.stage_pending_commit_unlocked(operation_id).unwrap();
        let path = store.operation_path(&operation_id);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_file_atomic(&path, &bytes).unwrap();

        let reopened = CollaborationStore::open(temp.path()).unwrap();
        assert_eq!(reopened.operation_ids().unwrap(), vec![operation_id]);
        assert!(reopened.verify_integrity().unwrap().index_current);
        assert!(!reopened.pending_commit_path().exists());
    }

    #[test]
    fn operation_paths_are_sharded_by_content_identity() {
        let temp = TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let outcome = store.write_operation(&operation("one")).unwrap();
        let path = store.operation_path(&outcome.operation_id);
        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            &outcome.operation_id.to_hex()[..2]
        );
        assert!(path.exists());
    }

    #[test]
    fn per_discussion_index_keeps_show_local() {
        let temp = TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let first = operation("one");
        let first_id = first.discussion_id;
        store.write_operation(&first).unwrap();
        let mut second = operation("two");
        second.discussion_id = "disc-018f47ea-4a54-7c89-b012-3456789abcdf".parse().unwrap();
        store.write_operation(&second).unwrap();

        let ids = store.discussion_operation_ids(&first_id).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            store
                .materialize_discussion(&first_id)
                .unwrap()
                .unwrap()
                .title,
            "Review one"
        );
    }

    #[test]
    fn lock_serializes_concurrent_idempotent_appends() {
        let temp = TempDir::new().unwrap();
        let store = Arc::new(CollaborationStore::open(temp.path()).unwrap());
        let operation = Arc::new(operation("same"));
        let handles = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                let operation = Arc::clone(&operation);
                thread::spawn(move || store.write_operation(&operation).unwrap())
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(store.operation_ids().unwrap().len(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.disposition == CollaborationWriteDisposition::Created)
                .count(),
            1
        );
    }

    #[test]
    fn replace_operations_retires_old_and_indexes_new() {
        let temp = TempDir::new().unwrap();
        let store = CollaborationStore::open(temp.path()).unwrap();
        let first = operation("local-only");
        let first_id = store.write_operation(&first).unwrap().operation_id;
        let replacement = operation("hosted-canonical");
        let outcomes = store
            .replace_operations(&[first_id], std::slice::from_ref(&replacement))
            .unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].disposition,
            CollaborationWriteDisposition::Created
        );
        let ids = store.operation_ids().unwrap();
        assert_eq!(ids, vec![outcomes[0].operation_id]);
        assert!(store.read_operation(&first_id).unwrap().is_none());
        assert_eq!(
            store
                .read_operation(&outcomes[0].operation_id)
                .unwrap()
                .unwrap()
                .operation
                .idempotency_key,
            replacement.idempotency_key
        );
        assert!(store.verify_integrity().unwrap().index_current);
    }
}
