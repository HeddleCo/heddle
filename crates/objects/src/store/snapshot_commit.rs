// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use serde::{Deserialize, Serialize};

use crate::{
    object::{ContentHash, StateId},
    store::{
        HeddleError, Result,
        pack::{ObjectType, PackManager, PackObjectId},
    },
};

pub const SNAPSHOT_COMMIT_ARTIFACT_SCHEMA: u32 = 1;

pub(crate) fn snapshot_commit_marker_path(pack_path: &Path, artifact_id: &ContentHash) -> PathBuf {
    let stem = pack_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    pack_path.with_file_name(format!("{stem}.snapshot-commit-{}", artifact_id.to_hex()))
}

/// Commit metadata embedded in the same durable pack as a structured snapshot.
/// The oplog and refs are materialized views of this record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCommitArtifact {
    pub schema: u32,
    pub transaction_id: String,
    pub scope: String,
    pub base_oplog_head_id: u64,
    pub state: StateId,
    /// Canonical encoded `OpRecord`s, including the transaction marker.
    pub encoded_records: Vec<Vec<u8>>,
}

/// Recovery descriptor retaining the enclosing content-addressed pack identity
/// without creating an impossible self-hash inside the pack payload.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct SnapshotCommitDescriptor {
    pub artifact: SnapshotCommitArtifact,
    pub pack_name: String,
    pub pack_path: PathBuf,
    pub object_ids: Vec<PackObjectId>,
}

impl SnapshotCommitArtifact {
    pub fn id(&self) -> ContentHash {
        ContentHash::compute(
            &rmp_serde::to_vec_named(self).expect("artifact encoding is infallible"),
        )
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != SNAPSHOT_COMMIT_ARTIFACT_SCHEMA {
            return Err(HeddleError::InvalidObject(format!(
                "unsupported snapshot commit artifact schema {}",
                self.schema
            )));
        }
        if self.transaction_id.is_empty() || self.encoded_records.is_empty() {
            return Err(HeddleError::InvalidObject(
                "snapshot commit artifact is incomplete".to_string(),
            ));
        }
        Ok(())
    }
}

/// Objects-owned seam around the format-only pack manager.
///
/// Snapshot commit decoding and indexing live here so `heddle-pack` remains
/// independent of objects-owned types and serializers.
#[doc(hidden)]
pub struct SnapshotPackManager {
    format: PackManager,
    snapshot_commit_index: OnceLock<std::result::Result<SnapshotCommitIndex, String>>,
}

struct SnapshotCommitIndex {
    descriptors: Vec<SnapshotCommitDescriptor>,
    by_state: HashMap<StateId, SnapshotCommitDescriptor>,
}

impl SnapshotPackManager {
    /// Open the format manager. The objects-owned snapshot index is built on
    /// its first query so ordinary repository opens do not decode every pack.
    pub fn new(packs_dir: PathBuf) -> Self {
        Self {
            format: PackManager::new(packs_dir),
            snapshot_commit_index: OnceLock::new(),
        }
    }

    /// Reload pack-format state and the objects-owned snapshot index.
    pub fn reload(&mut self) -> Result<()> {
        let mut format = PackManager::new(self.format.packs_dir().to_path_buf());
        format.reload()?;
        self.format = format;
        self.snapshot_commit_index = OnceLock::new();
        Ok(())
    }

    /// Reload both layers when the complete pack/index set changed on disk.
    pub fn reload_if_stale(&mut self) -> Result<bool> {
        if !self.format.needs_reload()? {
            return Ok(false);
        }
        self.reload()?;
        Ok(true)
    }

    pub(crate) fn add_pack(&mut self, pack_path: PathBuf, index_path: PathBuf) -> Result<()> {
        if self
            .format
            .pack_file_paths()
            .iter()
            .any(|(loaded_path, _)| *loaded_path == pack_path)
        {
            return Ok(());
        }
        self.format.add_pack(pack_path, index_path)?;
        self.snapshot_commit_index = OnceLock::new();
        Ok(())
    }

    pub(crate) fn snapshot_commit_descriptors(&self) -> Result<Vec<SnapshotCommitDescriptor>> {
        Ok(self.snapshot_commit_index()?.descriptors.clone())
    }

    pub(crate) fn snapshot_commit_descriptor_for_state(
        &self,
        state: &StateId,
    ) -> Result<Option<SnapshotCommitDescriptor>> {
        Ok(self.snapshot_commit_index()?.by_state.get(state).cloned())
    }

    fn snapshot_commit_index(&self) -> Result<&SnapshotCommitIndex> {
        match self.snapshot_commit_index.get_or_init(|| {
            Self::index_snapshot_commits(&self.format)
                .map(|(descriptors, by_state)| SnapshotCommitIndex {
                    descriptors,
                    by_state,
                })
                .map_err(|error| error.to_string())
        }) {
            Ok(index) => Ok(index),
            Err(error) => Err(HeddleError::InvalidObject(error.clone())),
        }
    }

    fn index_snapshot_commits(
        format: &PackManager,
    ) -> Result<(
        Vec<SnapshotCommitDescriptor>,
        HashMap<StateId, SnapshotCommitDescriptor>,
    )> {
        let mut descriptors = Vec::new();
        let mut by_state = HashMap::new();
        for (pack_path, _) in format.pack_file_paths() {
            for descriptor in Self::snapshot_commit_descriptors_for_pack(format, pack_path)? {
                by_state.insert(descriptor.artifact.state, descriptor.clone());
                descriptors.push(descriptor);
            }
        }
        Ok((descriptors, by_state))
    }

    fn snapshot_commit_descriptors_for_pack(
        format: &PackManager,
        pack_path: &Path,
    ) -> Result<Vec<SnapshotCommitDescriptor>> {
        let artifact_ids = snapshot_commit_marker_ids(pack_path)?;
        if artifact_ids.is_empty() {
            return Ok(Vec::new());
        }
        let object_ids = format.list_ids_from_pack(pack_path)?;
        let mut descriptors = Vec::new();
        for expected in artifact_ids {
            let id = PackObjectId::Hash(expected);
            let Some((ObjectType::SnapshotCommit, bytes)) =
                format.get_object_from_pack(pack_path, &id)?
            else {
                continue;
            };
            let artifact: SnapshotCommitArtifact = rmp_serde::from_slice(&bytes)?;
            artifact.validate()?;
            if artifact.id() != expected {
                return Err(HeddleError::InvalidObject(
                    "snapshot commit artifact address mismatch".to_string(),
                ));
            }
            descriptors.push(SnapshotCommitDescriptor {
                artifact,
                pack_name: pack_path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string(),
                pack_path: pack_path.to_path_buf(),
                object_ids: object_ids.clone(),
            });
        }
        Ok(descriptors)
    }
}

fn snapshot_commit_marker_ids(pack_path: &Path) -> Result<Vec<ContentHash>> {
    let Some(parent) = pack_path.parent() else {
        return Ok(Vec::new());
    };
    let stem = pack_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let prefix = format!("{stem}.snapshot-commit-");
    let mut ids = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        if let Ok(id) = ContentHash::from_hex(suffix) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

impl Deref for SnapshotPackManager {
    type Target = PackManager;

    fn deref(&self) -> &Self::Target {
        &self.format
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        time::Instant,
    };

    use heddle_format::compression::CompressionConfig;
    use tempfile::TempDir;

    use super::{
        SNAPSHOT_COMMIT_ARTIFACT_SCHEMA, SnapshotCommitArtifact, SnapshotPackManager,
        snapshot_commit_marker_path,
    };
    use crate::{
        object::{ContentHash, StateId},
        store::pack::{ObjectType, PackBuilder, PackManager},
    };

    fn write_snapshot_pack(root: &Path, ordinal: usize) -> (PathBuf, PathBuf, StateId) {
        let state = StateId::from_bytes([u8::try_from(ordinal + 1).unwrap(); 32]);
        let artifact = SnapshotCommitArtifact {
            schema: SNAPSHOT_COMMIT_ARTIFACT_SCHEMA,
            transaction_id: format!("tx-{ordinal}"),
            scope: "snapshot".to_string(),
            base_oplog_head_id: ordinal as u64,
            state,
            encoded_records: vec![vec![ordinal as u8]],
        };
        let artifact_id = artifact.id();
        let mut builder = PackBuilder::new(CompressionConfig {
            max_delta_size: 0,
            ..CompressionConfig::default()
        });
        builder.add(
            artifact_id,
            ObjectType::SnapshotCommit,
            rmp_serde::to_vec_named(&artifact).unwrap(),
        );
        let (pack_data, index_data, _) = builder.build().unwrap();
        let pack_path = root.join(format!("snapshot-{ordinal:03}.pack"));
        let index_path = root.join(format!("snapshot-{ordinal:03}.idx"));
        fs::write(&pack_path, pack_data).unwrap();
        fs::write(&index_path, index_data).unwrap();
        fs::write(snapshot_commit_marker_path(&pack_path, &artifact_id), []).unwrap();
        (pack_path, index_path, state)
    }

    #[test]
    fn objects_owned_pack_wrapper_preserves_pack_bytes_and_reads() {
        let temp = TempDir::new().unwrap();
        let packs_dir = temp.path().join("packs");
        fs::create_dir_all(&packs_dir).unwrap();

        let payload = b"issue-1122-pack-snapshot-seam".to_vec();
        let hash = ContentHash::compute(&payload);
        let mut builder = PackBuilder::new(CompressionConfig::disabled());
        builder.add(hash, ObjectType::Blob, payload.clone());
        let (pack_data, index_data, _) = builder.build().unwrap();

        assert_eq!(
            blake3::hash(&pack_data).to_hex().as_str(),
            "8a8f5a42787e958df6133989791f23363e7a736dca926fba5333aae1aee36c46",
            "pack bytes must match the v4 format baseline"
        );
        assert_eq!(
            blake3::hash(&index_data).to_hex().as_str(),
            "acd658d907bb8c561ec951ce149d4b93c60107640165d615e41c2411787db835",
            "index bytes must match the v4 format baseline"
        );

        let pack_path = packs_dir.join("fixture.pack");
        let index_path = packs_dir.join("fixture.idx");
        fs::write(&pack_path, &pack_data).unwrap();
        fs::write(&index_path, &index_data).unwrap();

        let format_manager = PackManager::new(packs_dir.clone());
        let mut snapshot_manager = SnapshotPackManager::new(packs_dir);
        assert_eq!(
            snapshot_manager.get_hashed_object(&hash).unwrap(),
            format_manager.get_hashed_object(&hash).unwrap()
        );
        assert_eq!(
            snapshot_manager.get_hashed_object(&hash).unwrap(),
            Some((ObjectType::Blob, payload))
        );

        snapshot_manager.reload().unwrap();
        assert_eq!(fs::read(pack_path).unwrap(), pack_data);
        assert_eq!(fs::read(index_path).unwrap(), index_data);
    }

    #[test]
    fn unmarked_pack_does_not_open_or_enumerate_its_index_for_recovery() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("ordinary.pack"), b"not opened").unwrap();
        fs::write(temp.path().join("ordinary.idx"), b"not opened").unwrap();

        let manager = SnapshotPackManager::new(temp.path().to_path_buf());
        assert!(manager.snapshot_commit_descriptors().unwrap().is_empty());
    }

    #[test]
    fn repeated_state_descriptor_lookup_stays_on_lazy_index_after_many_snapshots() {
        let temp = TempDir::new().unwrap();
        let mut manager = SnapshotPackManager::new(temp.path().to_path_buf());
        let mut states = Vec::new();
        for ordinal in 0..128 {
            let (pack_path, index_path, state) = write_snapshot_pack(temp.path(), ordinal);
            manager.add_pack(pack_path, index_path).unwrap();
            states.push(state);
        }
        assert_eq!(manager.pack_count(), 128);
        assert!(manager.snapshot_commit_index.get().is_none());
        assert_eq!(manager.snapshot_commit_descriptors().unwrap().len(), 128);
        assert!(manager.snapshot_commit_index.get().is_some());

        let started = Instant::now();
        for iteration in 0..100_000 {
            let state = states[iteration % states.len()];
            let descriptor = manager
                .snapshot_commit_descriptor_for_state(&state)
                .unwrap()
                .expect("every installed snapshot is indexed");
            assert_eq!(descriptor.artifact.state, state);
        }
        eprintln!(
            "100k cached snapshot descriptor lookups across 128 packs: {:?}",
            started.elapsed()
        );
        assert_eq!(
            manager.pack_count(),
            128,
            "lookup must not reload the pack set"
        );
    }
}
