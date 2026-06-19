// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for reference storage.
//!
//! The local CLI uses `RefManager` (disk-based). Hosted/server backends live
//! outside this crate and can implement this trait through
//! [`Repository::from_parts`](crate::Repository::from_parts).

use objects::{
    error::{HeddleError, Result},
    object::{ChangeId, RemoteName, Scope, ThreadName},
};

use super::{RefSummaryIndexInspection, RefUpdate, backend::CoreRefBackend};

/// Backend-agnostic interface for reading and writing repository references.
pub trait RefBackend: CoreRefBackend<Error = HeddleError> {
    fn get_remote_thread(
        &self,
        remote: &RemoteName,
        thread: &ThreadName,
    ) -> Result<Option<ChangeId>>;
    fn set_remote_thread(
        &self,
        remote: &RemoteName,
        thread: &ThreadName,
        state: &ChangeId,
    ) -> Result<()>;
    fn delete_remote_thread(
        &self,
        remote: &RemoteName,
        thread: &ThreadName,
    ) -> Result<Option<ChangeId>>;
    fn list_remotes(&self) -> Result<Vec<RemoteName>>;
    fn list_remote_threads(&self, remote: &RemoteName) -> Result<Vec<ThreadName>>;

    /// The write chokepoint (heddle#330 §2.2 r18): append the caller-supplied
    /// ref-carrying record batch (phase 4, opaque rmp-serde `OpRecord` bytes so
    /// `refs` names no `oplog` type) before publishing the atomic ref batch
    /// (phase 5), record-before-publish. The seam is **per backend**: the file
    /// backend (`RefManager`) earns atomicity by oplog-append-then-publish +
    /// per-read reconciliation.
    ///
    /// The default publishes **without** a record — backends with an injected
    /// committer override it.
    ///
    /// **Fail closed (heddle#354 r9, cid 3330304656).** A backend with no
    /// record committer MUST NOT publish the ref batch while silently dropping
    /// the records it was handed — committed data must never be lost. A
    /// record-free publish has nothing to lose and stays on the plain path; a
    /// publish carrying records is refused with an error so the caller learns
    /// the records would have vanished, rather than discovering it after the
    /// fact via a reconcile that folds an empty tail.
    fn commit_and_publish(
        &self,
        encoded_records: &[Vec<u8>],
        ref_updates: &[RefUpdate],
        scope: Option<&Scope>,
    ) -> Result<()> {
        let _ = scope;
        if !encoded_records.is_empty() {
            return Err(HeddleError::Config(format!(
                "commit_and_publish was handed {} record(s) on a backend with no record \
                 committer; refusing to publish and silently drop committed data",
                encoded_records.len()
            )));
        }
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
