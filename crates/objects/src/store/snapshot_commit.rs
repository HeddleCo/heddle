// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    object::{ContentHash, StateId},
    store::{HeddleError, Result, pack::PackObjectId},
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
