// SPDX-License-Identifier: Apache-2.0
//! The oplog-backed [`RefCommitter`] (heddle#330 write chokepoint).
//!
//! The write-side dual of [`OplogRefReconciler`](super::OplogRefReconciler):
//! appends the typed `OpRecord` batch handed across the `refs`→`repo` seam to
//! the file oplog (phase 4) before `RefManager` publishes the ref batch
//! (phase 5). Defined in `repo` and injected into `RefManager`, so the oplog
//! append stays next to the code that owns the file layout.

use std::path::{Path, PathBuf};

use objects::{
    error::Result,
    object::Principal,
};
use oplog::{OpLog, OpLogBackend, OpRecord};
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
    fn commit_records(&self, records: &[OpRecord], scope: Option<&str>) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        // heddle#382 boundary: ref commit-and-publish is not an AtomicMutation
        // transaction and does not append a TransactionCommit marker. Commands
        // needing same-thread isolation must enter through an AtomicMutation
        // root; this committer intentionally does not grow an ad hoc CAS layer.
        // Fresh handle so the append reloads the current log under the write
        // lock; preserves the configured principal for attribution.
        let oplog = OpLog::new(&self.heddle_dir, self.principal.clone());
        oplog.record_batch_scoped(records.to_vec(), scope)?;
        Ok(())
    }
}
