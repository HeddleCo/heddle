// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for reference storage.
//!
//! The local CLI uses `RefManager` (disk-based). The server uses `PgRefBackend`
//! (Postgres-backed). Both implement this trait so `Repository` can hold either.

use objects::{
    error::{HeddleError, Result},
    object::{ChangeId, ThreadName},
};

use super::{backend::CoreRefBackend, RefSummaryIndexInspection, RefUpdate};

/// Backend-agnostic interface for reading and writing repository references.
pub trait RefBackend: CoreRefBackend<Error = HeddleError> {
    fn get_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>>;
    fn set_remote_thread(&self, remote: &str, thread: &ThreadName, state: &ChangeId) -> Result<()>;
    fn delete_remote_thread(&self, remote: &str, thread: &ThreadName) -> Result<Option<ChangeId>>;
    fn list_remotes(&self) -> Result<Vec<String>>;
    fn list_remote_threads(&self, remote: &str) -> Result<Vec<ThreadName>>;

    /// The write chokepoint (heddle#330 §2.2 r18): append the caller-supplied
    /// ref-carrying record batch (phase 4, opaque rmp-serde `OpRecord` bytes so
    /// `refs` names no `oplog` type) before publishing the atomic ref batch
    /// (phase 5), record-before-publish. The seam is **per backend**: the file
    /// backend (`RefManager`) earns atomicity by oplog-append-then-publish +
    /// per-read reconciliation; the Postgres backend would co-commit the record
    /// row and ref/head rows in one SQL transaction (native ACID).
    ///
    /// The default publishes **without** a record — backends with an injected
    /// committer override it. (`PgRefBackend`'s single-tx co-commit needs the
    /// server's shared-pool wiring, which is out of this tree; it keeps the
    /// default until that lands.)
    fn commit_and_publish(
        &self,
        encoded_records: &[Vec<u8>],
        ref_updates: &[RefUpdate],
        scope: Option<&str>,
    ) -> Result<()> {
        let _ = (encoded_records, scope);
        self.update_refs(ref_updates)
    }

    fn inspect_ref_summary_index(&self) -> Result<RefSummaryIndexInspection> {
        Ok(RefSummaryIndexInspection::absent())
    }

    fn rebuild_ref_summary_index(&self) -> Result<RefSummaryIndexInspection> {
        Ok(RefSummaryIndexInspection::absent())
    }

    fn pack_refs(&self) -> Result<()> {
        Ok(())
    }

    fn cleanup_stale_temps(&self) {}
}
