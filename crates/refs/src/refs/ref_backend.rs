// SPDX-License-Identifier: Apache-2.0
//! Abstract backend trait for reference storage.
//!
//! The local CLI uses `RefManager` (disk-based). The server uses `PgRefBackend`
//! (Postgres-backed). Both implement this trait so `Repository` can hold either.

use objects::{
    error::{HeddleError, Result},
    object::ChangeId,
};

use super::{RefSummaryIndexInspection, backend::CoreRefBackend};

/// Backend-agnostic interface for reading and writing repository references.
pub trait RefBackend: CoreRefBackend<Error = HeddleError> {
    fn get_remote_thread(&self, remote: &str, thread: &str) -> Result<Option<ChangeId>>;
    fn set_remote_thread(&self, remote: &str, thread: &str, state: &ChangeId) -> Result<()>;
    fn delete_remote_thread(&self, remote: &str, thread: &str) -> Result<Option<ChangeId>>;
    fn list_remotes(&self) -> Result<Vec<String>>;
    fn list_remote_threads(&self, remote: &str) -> Result<Vec<String>>;

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