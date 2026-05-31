// SPDX-License-Identifier: Apache-2.0
//! The oplog-backed [`RefCommitter`] (heddle#330 write chokepoint).
//!
//! The write-side dual of [`OplogRefReconciler`](super::OplogRefReconciler):
//! decodes the opaque `OpRecord` batch handed across the `refs`→`repo` seam and
//! appends it to the file oplog (phase 4) before `RefManager` publishes the ref
//! batch (phase 5). Defined in `repo` (which names `OpRecord`) and injected into
//! `RefManager`, so `refs` keeps no `oplog` dependency.

use std::path::{Path, PathBuf};

use objects::error::{HeddleError, Result};
use objects::object::Principal;
use oplog::{OpLog, OpRecord};
use refs::RefCommitter;

/// Appends ref-carrying records to the file oplog as the phase-4 commit point.
pub struct OplogRefCommitter {
    heddle_dir: PathBuf,
    principal: Principal,
}

impl OplogRefCommitter {
    pub fn new(heddle_dir: &Path, principal: Principal) -> Self {
        Self {
            heddle_dir: heddle_dir.to_path_buf(),
            principal,
        }
    }
}

impl RefCommitter for OplogRefCommitter {
    fn commit_records(&self, encoded_records: &[Vec<u8>], scope: Option<&str>) -> Result<()> {
        if encoded_records.is_empty() {
            return Ok(());
        }
        let records = encoded_records
            .iter()
            .map(|bytes| {
                rmp_serde::from_slice::<OpRecord>(bytes)
                    .map_err(|e| HeddleError::Serialization(e.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        // Fresh handle so the append reloads the current log under the write
        // lock; preserves the configured principal for attribution.
        let oplog = OpLog::new(&self.heddle_dir, self.principal.clone());
        oplog.record_batch_scoped(records, scope)?;
        Ok(())
    }
}
