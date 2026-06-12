// SPDX-License-Identifier: Apache-2.0
//! Versioned codecs for [`OpRecord`] payloads stored inside the packed oplog.
//!
//! The packed oplog container has its own magic/version header, but each entry
//! also embeds an rmp-serde `OpRecord` payload. These payloads are a separate
//! schema surface: changing the fields of an existing `OpRecord` variant changes
//! the bytes while leaving the container untouched.
//!
//! Invariant for future record-schema changes:
//! - bump [`LatestOpRecordSchema::VERSION`] by adding a new sealed schema type,
//! - keep the older schema as a frozen decode-only snapshot,
//! - map that older schema into the canonical in-memory [`OpRecord`], and
//! - make packed-oplog migration rewrite old bytes with the latest version.
//!
//! Decoders select by a stored schema version for current files. The
//! `decode_unversioned` path exists only for pre-versioned v2/v3 oplogs and
//! tries frozen schemas explicitly; new formats must never blind-deserialize a
//! payload without a schema version.
//!
//! Documented exception (#352, pre-v0.3.0): the V2 OpRecord variant collapse
//! rewrote the legacy schema mirrors to the collapsed shapes instead of
//! keeping frozen V1/V2 arms — accepted under the no-production-oplogs
//! premise. Old dev oplogs containing thread-create/fast-forward records do
//! not decode past this point. This exception must not be repeated once
//! public binaries exist.

use objects::{
    error::{HeddleError, Result},
    object::{ChangeId, ContentHash, VisibilityTier},
};
use serde::{Deserialize, Serialize};

use super::oplog_types::{OpRecord, ThreadUpdateSnapshots};

pub(crate) const LATEST_RECORD_SCHEMA_VERSION: u32 = LatestOpRecordSchema::VERSION;

mod sealed {
    pub trait Sealed {}
}

pub(crate) trait VersionedOpRecordSchema: sealed::Sealed {
    const VERSION: u32;
    const NAME: &'static str;

    fn decode(bytes: &[u8]) -> Result<OpRecord>;
}

pub(crate) struct PreAtomicOpRecordSchema;
pub(crate) struct AtomicNoHeadOpRecordSchema;
pub(crate) struct CurrentOpRecordSchema;
pub(crate) type LatestOpRecordSchema = CurrentOpRecordSchema;

impl sealed::Sealed for PreAtomicOpRecordSchema {}
impl sealed::Sealed for AtomicNoHeadOpRecordSchema {}
impl sealed::Sealed for CurrentOpRecordSchema {}

impl VersionedOpRecordSchema for PreAtomicOpRecordSchema {
    const VERSION: u32 = 1;
    const NAME: &'static str = "pre-atomic-v1";

    fn decode(bytes: &[u8]) -> Result<OpRecord> {
        let record: PreAtomicOpRecord = decode_rmp(bytes, Self::NAME)?;
        Ok(record.into_current())
    }
}

impl VersionedOpRecordSchema for AtomicNoHeadOpRecordSchema {
    const VERSION: u32 = 2;
    const NAME: &'static str = "atomic-no-head-v2";

    fn decode(bytes: &[u8]) -> Result<OpRecord> {
        let record: AtomicNoHeadOpRecord = decode_rmp(bytes, Self::NAME)?;
        Ok(record.into_current())
    }
}

impl VersionedOpRecordSchema for CurrentOpRecordSchema {
    const VERSION: u32 = 3;
    const NAME: &'static str = "current-v3";

    fn decode(bytes: &[u8]) -> Result<OpRecord> {
        let record: StrictCurrentOpRecord = decode_rmp(bytes, Self::NAME)?;
        Ok(record.into_current())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpRecordSchemaVersion {
    PreAtomic,
    AtomicNoHead,
    Current,
}

impl OpRecordSchemaVersion {
    pub(crate) fn number(self) -> u32 {
        match self {
            Self::PreAtomic => PreAtomicOpRecordSchema::VERSION,
            Self::AtomicNoHead => AtomicNoHeadOpRecordSchema::VERSION,
            Self::Current => CurrentOpRecordSchema::VERSION,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::PreAtomic => PreAtomicOpRecordSchema::NAME,
            Self::AtomicNoHead => AtomicNoHeadOpRecordSchema::NAME,
            Self::Current => CurrentOpRecordSchema::NAME,
        }
    }
}

pub(crate) fn schema_version_from_u32(version: u32) -> Result<OpRecordSchemaVersion> {
    match version {
        PreAtomicOpRecordSchema::VERSION => Ok(OpRecordSchemaVersion::PreAtomic),
        AtomicNoHeadOpRecordSchema::VERSION => Ok(OpRecordSchemaVersion::AtomicNoHead),
        CurrentOpRecordSchema::VERSION => Ok(OpRecordSchemaVersion::Current),
        other => Err(HeddleError::InvalidObject(format!(
            "unsupported OpRecord schema version {other}"
        ))),
    }
}

pub(crate) fn decode_versioned_record(
    bytes: &[u8],
    version: OpRecordSchemaVersion,
) -> Result<OpRecord> {
    match version {
        OpRecordSchemaVersion::PreAtomic => PreAtomicOpRecordSchema::decode(bytes),
        OpRecordSchemaVersion::AtomicNoHead => AtomicNoHeadOpRecordSchema::decode(bytes),
        OpRecordSchemaVersion::Current => CurrentOpRecordSchema::decode(bytes),
    }
}

pub(crate) fn encode_latest_record(record: &OpRecord) -> Result<Vec<u8>> {
    rmp_serde::to_vec(record).map_err(|e| HeddleError::Serialization(e.to_string()))
}

pub(crate) fn candidate_versions_newest_first() -> [OpRecordSchemaVersion; 3] {
    [
        OpRecordSchemaVersion::Current,
        OpRecordSchemaVersion::AtomicNoHead,
        OpRecordSchemaVersion::PreAtomic,
    ]
}

fn decode_rmp<T>(bytes: &[u8], schema_name: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    rmp_serde::from_slice(bytes).map_err(|e| {
        HeddleError::Serialization(format!(
            "failed to decode OpRecord payload as {schema_name}: {e}"
        ))
    })
}

/// Strict frozen snapshot of the current record schema.
///
/// `OpRecord` keeps `serde(default)` on some tail fields for general
/// compatibility, but unversioned schema probing must not let legacy-short
/// reshaped variants decode as current. This mirror has no defaults: a current
/// `Snapshot` must carry `head`, a current `Goto` must carry `head`, and current
/// `Fork`/`Collapse` must carry their published-ref tail fields.
#[derive(Debug, Clone, Deserialize)]
enum StrictCurrentOpRecord {
    Snapshot {
        new_state: ChangeId,
        prev_head: Option<ChangeId>,
        head: Option<ChangeId>,
        thread: Option<String>,
    },
    Goto {
        target: ChangeId,
        prev_head: Option<ChangeId>,
        head: ChangeId,
    },
    ThreadCreate {
        name: String,
        state: ChangeId,
        manager_snapshot: Option<Vec<u8>>,
    },
    ThreadDelete {
        name: String,
        state: ChangeId,
    },
    ThreadUpdate {
        name: String,
        old_state: ChangeId,
        new_state: ChangeId,
        #[serde(default)]
        manager_snapshots: Option<ThreadUpdateSnapshots>,
    },
    Fork {
        from: ChangeId,
        new_state: ChangeId,
        thread: Option<String>,
        head: Option<ChangeId>,
    },
    Collapse {
        sources: Vec<ChangeId>,
        result: ChangeId,
        thread: Option<String>,
    },
    MarkerCreate {
        name: String,
        state: ChangeId,
    },
    MarkerDelete {
        name: String,
        state: ChangeId,
    },
    Checkpoint {
        parent: Option<ChangeId>,
        state: ChangeId,
        thread: Option<String>,
    },
    TransactionAbort {
        transaction_id: String,
        reason: String,
    },
    EphemeralThreadCollapse {
        thread: String,
        final_state: ChangeId,
    },
    ConflictResolved {
        conflict_id: String,
        resolution: String,
    },
    TransactionCommit {
        transaction_id: String,
        op_count: u32,
    },
    Redact {
        redaction_id: ContentHash,
        blob: ContentHash,
        state: ChangeId,
        path: String,
    },
    Purge {
        redaction_id: ContentHash,
        blob: ContentHash,
    },
    FastForward {
        source_thread: String,
        target_thread: String,
        pre_target_id: ChangeId,
        post_target_id: ChangeId,
    },
    GitCheckpoint {
        branch: String,
        state: ChangeId,
        previous_git_oid: Option<String>,
        new_git_oid: String,
    },
    RemoteThreadUpdate {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    RemoteThreadDelete {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    UndoRecoveryUpdate {
        state: ChangeId,
    },
    StateVisibilitySet {
        state: ChangeId,
        record_id: ContentHash,
        tier: VisibilityTier,
        #[serde(default)]
        prior_sidecar: Option<Vec<u8>>,
        #[serde(default)]
        new_sidecar: Option<Vec<u8>>,
    },
    StateVisibilityPromote {
        state: ChangeId,
        superseded: ContentHash,
        record_id: ContentHash,
        tier: VisibilityTier,
        #[serde(default)]
        prior_sidecar: Option<Vec<u8>>,
        #[serde(default)]
        new_sidecar: Option<Vec<u8>>,
    },
}

impl StrictCurrentOpRecord {
    fn into_current(self) -> OpRecord {
        match self {
            Self::Snapshot {
                new_state,
                prev_head,
                head,
                thread,
            } => OpRecord::Snapshot {
                new_state,
                prev_head,
                head,
                thread,
            },
            Self::Goto {
                target,
                prev_head,
                head,
            } => OpRecord::Goto {
                target,
                prev_head,
                head,
            },
            Self::ThreadCreate {
                name,
                state,
                manager_snapshot,
            } => OpRecord::ThreadCreate {
                name,
                state,
                manager_snapshot,
            },
            Self::ThreadDelete { name, state } => OpRecord::ThreadDelete { name, state },
            Self::ThreadUpdate {
                name,
                old_state,
                new_state,
                manager_snapshots,
            } => OpRecord::ThreadUpdate {
                name,
                old_state,
                new_state,
                manager_snapshots,
            },
            Self::Fork {
                from,
                new_state,
                thread,
                head,
            } => OpRecord::Fork {
                from,
                new_state,
                thread,
                head,
            },
            Self::Collapse {
                sources,
                result,
                thread,
            } => OpRecord::Collapse {
                sources,
                result,
                thread,
            },
            Self::MarkerCreate { name, state } => OpRecord::MarkerCreate { name, state },
            Self::MarkerDelete { name, state } => OpRecord::MarkerDelete { name, state },
            Self::Checkpoint {
                parent,
                state,
                thread,
            } => OpRecord::Checkpoint {
                parent,
                state,
                thread,
            },
            Self::TransactionAbort {
                transaction_id,
                reason,
            } => OpRecord::TransactionAbort {
                transaction_id,
                reason,
            },
            Self::EphemeralThreadCollapse {
                thread,
                final_state,
            } => OpRecord::EphemeralThreadCollapse {
                thread,
                final_state,
            },
            Self::ConflictResolved {
                conflict_id,
                resolution,
            } => OpRecord::ConflictResolved {
                conflict_id,
                resolution,
            },
            Self::TransactionCommit {
                transaction_id,
                op_count,
            } => OpRecord::TransactionCommit {
                transaction_id,
                op_count,
            },
            Self::Redact {
                redaction_id,
                blob,
                state,
                path,
            } => OpRecord::Redact {
                redaction_id,
                blob,
                state,
                path,
            },
            Self::Purge { redaction_id, blob } => OpRecord::Purge { redaction_id, blob },
            Self::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            } => OpRecord::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            },
            Self::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            } => OpRecord::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            },
            Self::RemoteThreadUpdate {
                remote,
                thread,
                state,
            } => OpRecord::RemoteThreadUpdate {
                remote,
                thread,
                state,
            },
            Self::RemoteThreadDelete {
                remote,
                thread,
                state,
            } => OpRecord::RemoteThreadDelete {
                remote,
                thread,
                state,
            },
            Self::UndoRecoveryUpdate { state } => OpRecord::UndoRecoveryUpdate { state },
            Self::StateVisibilitySet {
                state,
                record_id,
                tier,
                prior_sidecar,
                new_sidecar,
            } => OpRecord::StateVisibilitySet {
                state,
                record_id,
                tier,
                prior_sidecar,
                new_sidecar,
            },
            Self::StateVisibilityPromote {
                state,
                superseded,
                record_id,
                tier,
                prior_sidecar,
                new_sidecar,
            } => OpRecord::StateVisibilityPromote {
                state,
                superseded,
                record_id,
                tier,
                prior_sidecar,
                new_sidecar,
            },
        }
    }
}

/// Frozen `OpRecord` snapshot from immediately before 7125992
/// (heddle#354 AtomicMutation).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum PreAtomicOpRecord {
    Snapshot {
        new_state: ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<String>,
    },
    Goto {
        target: ChangeId,
        prev_head: Option<ChangeId>,
    },
    ThreadCreate {
        name: String,
        state: ChangeId,
        manager_snapshot: Option<Vec<u8>>,
    },
    ThreadDelete {
        name: String,
        state: ChangeId,
    },
    ThreadUpdate {
        name: String,
        old_state: ChangeId,
        new_state: ChangeId,
    },
    Fork {
        from: ChangeId,
        new_state: ChangeId,
    },
    Collapse {
        sources: Vec<ChangeId>,
        result: ChangeId,
    },
    MarkerCreate {
        name: String,
        state: ChangeId,
    },
    MarkerDelete {
        name: String,
        state: ChangeId,
    },
    Checkpoint {
        parent: Option<ChangeId>,
        state: ChangeId,
        thread: Option<String>,
    },
    TransactionAbort {
        transaction_id: String,
        reason: String,
    },
    EphemeralThreadCollapse {
        thread: String,
        final_state: ChangeId,
    },
    ConflictResolved {
        conflict_id: String,
        resolution: String,
    },
    TransactionCommit {
        transaction_id: String,
        op_count: u32,
    },
    Redact {
        redaction_id: ContentHash,
        blob: ContentHash,
        state: ChangeId,
        path: String,
    },
    Purge {
        redaction_id: ContentHash,
        blob: ContentHash,
    },
    FastForward {
        source_thread: String,
        target_thread: String,
        pre_target_id: ChangeId,
        post_target_id: ChangeId,
    },
    GitCheckpoint {
        branch: String,
        state: ChangeId,
        previous_git_oid: Option<String>,
        new_git_oid: String,
    },
}

impl PreAtomicOpRecord {
    fn into_current(self) -> OpRecord {
        match self {
            Self::Snapshot {
                new_state,
                prev_head,
                thread,
            } => OpRecord::Snapshot {
                new_state,
                prev_head,
                head: thread.is_none().then_some(new_state),
                thread,
            },
            Self::Goto { target, prev_head } => OpRecord::Goto {
                target,
                prev_head,
                head: target,
            },
            Self::ThreadCreate {
                name,
                state,
                manager_snapshot,
            } => OpRecord::ThreadCreate {
                name,
                state,
                manager_snapshot,
            },
            Self::ThreadDelete { name, state } => OpRecord::ThreadDelete { name, state },
            Self::ThreadUpdate {
                name,
                old_state,
                new_state,
            } => OpRecord::ThreadUpdate {
                name,
                old_state,
                new_state,
                manager_snapshots: None,
            },
            Self::Fork { from, new_state } => {
                // The pre-Atomic CLI was the only caller and recorded these two
                // fields reversed. Preserve the historical operation meaning.
                OpRecord::Fork {
                    from: new_state,
                    new_state: from,
                    thread: None,
                    head: None,
                }
            }
            Self::Collapse { sources, result } => OpRecord::Collapse {
                sources,
                result,
                thread: None,
            },
            Self::MarkerCreate { name, state } => OpRecord::MarkerCreate { name, state },
            Self::MarkerDelete { name, state } => OpRecord::MarkerDelete { name, state },
            Self::Checkpoint {
                parent,
                state,
                thread,
            } => OpRecord::Checkpoint {
                parent,
                state,
                thread,
            },
            Self::TransactionAbort {
                transaction_id,
                reason,
            } => OpRecord::TransactionAbort {
                transaction_id,
                reason,
            },
            Self::EphemeralThreadCollapse {
                thread,
                final_state,
            } => OpRecord::EphemeralThreadCollapse {
                thread,
                final_state,
            },
            Self::ConflictResolved {
                conflict_id,
                resolution,
            } => OpRecord::ConflictResolved {
                conflict_id,
                resolution,
            },
            Self::TransactionCommit {
                transaction_id,
                op_count,
            } => OpRecord::TransactionCommit {
                transaction_id,
                op_count,
            },
            Self::Redact {
                redaction_id,
                blob,
                state,
                path,
            } => OpRecord::Redact {
                redaction_id,
                blob,
                state,
                path,
            },
            Self::Purge { redaction_id, blob } => OpRecord::Purge { redaction_id, blob },
            Self::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            } => OpRecord::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            },
            Self::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            } => OpRecord::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            },
        }
    }
}

/// Frozen `OpRecord` snapshot after 7125992 and before 82a9e79. It has the
/// AtomicMutation published-ref fields for `Fork`/`Collapse`, but
/// `Snapshot`/`Goto` do not yet carry their published HEAD fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum AtomicNoHeadOpRecord {
    Snapshot {
        new_state: ChangeId,
        prev_head: Option<ChangeId>,
        thread: Option<String>,
    },
    Goto {
        target: ChangeId,
        prev_head: Option<ChangeId>,
    },
    ThreadCreate {
        name: String,
        state: ChangeId,
        manager_snapshot: Option<Vec<u8>>,
    },
    ThreadDelete {
        name: String,
        state: ChangeId,
    },
    ThreadUpdate {
        name: String,
        old_state: ChangeId,
        new_state: ChangeId,
    },
    Fork {
        from: ChangeId,
        new_state: ChangeId,
        thread: Option<String>,
        head: Option<ChangeId>,
    },
    Collapse {
        sources: Vec<ChangeId>,
        result: ChangeId,
        thread: Option<String>,
    },
    MarkerCreate {
        name: String,
        state: ChangeId,
    },
    MarkerDelete {
        name: String,
        state: ChangeId,
    },
    Checkpoint {
        parent: Option<ChangeId>,
        state: ChangeId,
        thread: Option<String>,
    },
    TransactionAbort {
        transaction_id: String,
        reason: String,
    },
    EphemeralThreadCollapse {
        thread: String,
        final_state: ChangeId,
    },
    ConflictResolved {
        conflict_id: String,
        resolution: String,
    },
    TransactionCommit {
        transaction_id: String,
        op_count: u32,
    },
    Redact {
        redaction_id: ContentHash,
        blob: ContentHash,
        state: ChangeId,
        path: String,
    },
    Purge {
        redaction_id: ContentHash,
        blob: ContentHash,
    },
    FastForward {
        source_thread: String,
        target_thread: String,
        pre_target_id: ChangeId,
        post_target_id: ChangeId,
    },
    GitCheckpoint {
        branch: String,
        state: ChangeId,
        previous_git_oid: Option<String>,
        new_git_oid: String,
    },
    RemoteThreadUpdate {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    RemoteThreadDelete {
        remote: String,
        thread: String,
        state: ChangeId,
    },
    UndoRecoveryUpdate {
        state: ChangeId,
    },
}

impl AtomicNoHeadOpRecord {
    fn into_current(self) -> OpRecord {
        match self {
            Self::Snapshot {
                new_state,
                prev_head,
                thread,
            } => OpRecord::Snapshot {
                new_state,
                prev_head,
                head: thread.is_none().then_some(new_state),
                thread,
            },
            Self::Goto { target, prev_head } => OpRecord::Goto {
                target,
                prev_head,
                head: target,
            },
            Self::ThreadCreate {
                name,
                state,
                manager_snapshot,
            } => OpRecord::ThreadCreate {
                name,
                state,
                manager_snapshot,
            },
            Self::ThreadDelete { name, state } => OpRecord::ThreadDelete { name, state },
            Self::ThreadUpdate {
                name,
                old_state,
                new_state,
            } => OpRecord::ThreadUpdate {
                name,
                old_state,
                new_state,
                manager_snapshots: None,
            },
            Self::Fork {
                from,
                new_state,
                thread,
                head,
            } => OpRecord::Fork {
                from,
                new_state,
                thread,
                head,
            },
            Self::Collapse {
                sources,
                result,
                thread,
            } => OpRecord::Collapse {
                sources,
                result,
                thread,
            },
            Self::MarkerCreate { name, state } => OpRecord::MarkerCreate { name, state },
            Self::MarkerDelete { name, state } => OpRecord::MarkerDelete { name, state },
            Self::Checkpoint {
                parent,
                state,
                thread,
            } => OpRecord::Checkpoint {
                parent,
                state,
                thread,
            },
            Self::TransactionAbort {
                transaction_id,
                reason,
            } => OpRecord::TransactionAbort {
                transaction_id,
                reason,
            },
            Self::EphemeralThreadCollapse {
                thread,
                final_state,
            } => OpRecord::EphemeralThreadCollapse {
                thread,
                final_state,
            },
            Self::ConflictResolved {
                conflict_id,
                resolution,
            } => OpRecord::ConflictResolved {
                conflict_id,
                resolution,
            },
            Self::TransactionCommit {
                transaction_id,
                op_count,
            } => OpRecord::TransactionCommit {
                transaction_id,
                op_count,
            },
            Self::Redact {
                redaction_id,
                blob,
                state,
                path,
            } => OpRecord::Redact {
                redaction_id,
                blob,
                state,
                path,
            },
            Self::Purge { redaction_id, blob } => OpRecord::Purge { redaction_id, blob },
            Self::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            } => OpRecord::FastForward {
                source_thread,
                target_thread,
                pre_target_id,
                post_target_id,
            },
            Self::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            } => OpRecord::GitCheckpoint {
                branch,
                state,
                previous_git_oid,
                new_git_oid,
            },
            Self::RemoteThreadUpdate {
                remote,
                thread,
                state,
            } => OpRecord::RemoteThreadUpdate {
                remote,
                thread,
                state,
            },
            Self::RemoteThreadDelete {
                remote,
                thread,
                state,
            } => OpRecord::RemoteThreadDelete {
                remote,
                thread,
                state,
            },
            Self::UndoRecoveryUpdate { state } => OpRecord::UndoRecoveryUpdate { state },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tests_support, *};

    fn cid(byte: u8) -> ChangeId {
        ChangeId::from_bytes([byte; 16])
    }

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn assert_same_record(left: &OpRecord, right: &OpRecord) {
        assert_eq!(format!("{left:?}"), format!("{right:?}"));
    }

    fn pre_atomic_supported_records() -> Vec<OpRecord> {
        vec![
            OpRecord::Snapshot {
                new_state: cid(1),
                prev_head: None,
                head: Some(cid(1)),
                thread: None,
            },
            OpRecord::Snapshot {
                new_state: cid(2),
                prev_head: Some(cid(1)),
                head: None,
                thread: Some("main".into()),
            },
            OpRecord::Goto {
                target: cid(3),
                prev_head: Some(cid(2)),
                head: cid(3),
            },
            OpRecord::ThreadCreate {
                name: "topic".into(),
                state: cid(4),
                manager_snapshot: Some(vec![1, 2, 3]),
            },
            OpRecord::ThreadDelete {
                name: "old".into(),
                state: cid(5),
            },
            OpRecord::ThreadUpdate {
                name: "main".into(),
                old_state: cid(6),
                new_state: cid(7),
                manager_snapshots: None,
            },
            OpRecord::Fork {
                from: cid(8),
                new_state: cid(9),
                thread: None,
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![cid(8), cid(9)],
                result: cid(10),
                thread: None,
            },
            OpRecord::MarkerCreate {
                name: "release".into(),
                state: cid(11),
            },
            OpRecord::MarkerDelete {
                name: "draft".into(),
                state: cid(12),
            },
            OpRecord::Checkpoint {
                parent: Some(cid(12)),
                state: cid(13),
                thread: Some("main".into()),
            },
            OpRecord::TransactionAbort {
                transaction_id: "abort".into(),
                reason: "reason".into(),
            },
            OpRecord::EphemeralThreadCollapse {
                thread: "ephemeral".into(),
                final_state: cid(14),
            },
            OpRecord::ConflictResolved {
                conflict_id: "conflict".into(),
                resolution: "ours".into(),
            },
            OpRecord::TransactionCommit {
                transaction_id: "tx".into(),
                op_count: 2,
            },
            OpRecord::Redact {
                redaction_id: hash(1),
                blob: hash(2),
                state: cid(15),
                path: "secret.txt".into(),
            },
            OpRecord::Purge {
                redaction_id: hash(3),
                blob: hash(4),
            },
            OpRecord::FastForward {
                source_thread: "feature".into(),
                target_thread: "main".into(),
                pre_target_id: cid(17),
                post_target_id: cid(18),
            },
            OpRecord::GitCheckpoint {
                branch: "main".into(),
                state: cid(20),
                previous_git_oid: Some("abc".into()),
                new_git_oid: "def".into(),
            },
        ]
    }

    fn atomic_no_head_records() -> Vec<OpRecord> {
        let mut records = pre_atomic_supported_records();
        records.push(OpRecord::Fork {
            from: cid(21),
            new_state: cid(22),
            thread: Some("topic".into()),
            head: None,
        });
        records.push(OpRecord::Collapse {
            sources: vec![cid(22)],
            result: cid(23),
            thread: Some("main".into()),
        });
        records.push(OpRecord::RemoteThreadUpdate {
            remote: "origin".into(),
            thread: "main".into(),
            state: cid(24),
        });
        records.push(OpRecord::RemoteThreadDelete {
            remote: "origin".into(),
            thread: "old".into(),
            state: cid(25),
        });
        records.push(OpRecord::UndoRecoveryUpdate { state: cid(26) });
        records
    }

    #[test]
    fn schema_version_numbers_are_explicit() {
        assert_eq!(LATEST_RECORD_SCHEMA_VERSION, CurrentOpRecordSchema::VERSION);
        assert_eq!(
            schema_version_from_u32(PreAtomicOpRecordSchema::VERSION).unwrap(),
            OpRecordSchemaVersion::PreAtomic
        );
        assert_eq!(
            schema_version_from_u32(AtomicNoHeadOpRecordSchema::VERSION).unwrap(),
            OpRecordSchemaVersion::AtomicNoHead
        );
        assert_eq!(
            schema_version_from_u32(CurrentOpRecordSchema::VERSION).unwrap(),
            OpRecordSchemaVersion::Current
        );
        assert!(schema_version_from_u32(99).is_err());
        assert_eq!(
            candidate_versions_newest_first()
                .into_iter()
                .map(OpRecordSchemaVersion::number)
                .collect::<Vec<_>>(),
            vec![
                CurrentOpRecordSchema::VERSION,
                AtomicNoHeadOpRecordSchema::VERSION,
                PreAtomicOpRecordSchema::VERSION,
            ]
        );
    }

    #[test]
    fn current_nil_head_snapshot_serializes_required_nil_and_preserves_thread() {
        let record = OpRecord::Snapshot {
            new_state: cid(1),
            prev_head: Some(cid(2)),
            head: None,
            thread: Some("main".into()),
        };
        let bytes = encode_latest_record(&record).unwrap();

        let current = CurrentOpRecordSchema::decode(&bytes).unwrap();
        assert_same_record(&current, &record);
        assert!(PreAtomicOpRecordSchema::decode(&bytes).is_err());
        assert!(AtomicNoHeadOpRecordSchema::decode(&bytes).is_err());
    }

    #[test]
    fn migration_sensitive_legacy_shapes_do_not_decode_as_current() {
        let pre_atomic_reshaped = [
            OpRecord::Snapshot {
                new_state: cid(1),
                prev_head: None,
                head: Some(cid(1)),
                thread: None,
            },
            OpRecord::Snapshot {
                new_state: cid(2),
                prev_head: Some(cid(1)),
                head: None,
                thread: Some("main".into()),
            },
            OpRecord::Goto {
                target: cid(3),
                prev_head: Some(cid(2)),
                head: cid(3),
            },
            OpRecord::Fork {
                from: cid(4),
                new_state: cid(5),
                thread: None,
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![cid(4), cid(5)],
                result: cid(6),
                thread: None,
            },
        ];
        for legacy in pre_atomic_reshaped {
            let bytes = tests_support::encode_pre_atomic(&legacy).unwrap();
            assert!(
                CurrentOpRecordSchema::decode(&bytes).is_err(),
                "pre-atomic {legacy:?} must not decode as current"
            );
        }

        let atomic_no_head_requires_head = [
            OpRecord::Snapshot {
                new_state: cid(7),
                prev_head: None,
                head: Some(cid(7)),
                thread: None,
            },
            OpRecord::Snapshot {
                new_state: cid(8),
                prev_head: Some(cid(7)),
                head: None,
                thread: Some("main".into()),
            },
            OpRecord::Goto {
                target: cid(9),
                prev_head: Some(cid(8)),
                head: cid(9),
            },
        ];
        for legacy in atomic_no_head_requires_head {
            let bytes = tests_support::encode_atomic_no_head(&legacy).unwrap();
            assert!(
                CurrentOpRecordSchema::decode(&bytes).is_err(),
                "atomic-no-head {legacy:?} must not decode as current"
            );
        }
    }

    #[test]
    fn pre_atomic_fork_and_collapse_do_not_decode_as_atomic_no_head() {
        let pre_atomic_only_shapes = [
            OpRecord::Fork {
                from: cid(1),
                new_state: cid(2),
                thread: None,
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![cid(1), cid(2)],
                result: cid(3),
                thread: None,
            },
        ];

        for legacy in pre_atomic_only_shapes {
            let bytes = tests_support::encode_pre_atomic(&legacy).unwrap();
            assert!(
                AtomicNoHeadOpRecordSchema::decode(&bytes).is_err(),
                "pre-atomic {legacy:?} must fall through atomic-no-head probing"
            );
        }
    }

    #[test]
    fn atomic_no_head_same_shape_tail_records_decode_identically_as_current() {
        let same_shape_records = [
            OpRecord::Fork {
                from: cid(10),
                new_state: cid(11),
                thread: Some("topic".into()),
                head: None,
            },
            OpRecord::Fork {
                from: cid(12),
                new_state: cid(13),
                thread: None,
                head: Some(cid(13)),
            },
            OpRecord::Collapse {
                sources: vec![cid(10), cid(11)],
                result: cid(14),
                thread: Some("main".into()),
            },
            OpRecord::RemoteThreadUpdate {
                remote: "origin".into(),
                thread: "main".into(),
                state: cid(15),
            },
            OpRecord::RemoteThreadDelete {
                remote: "origin".into(),
                thread: "old".into(),
                state: cid(16),
            },
            OpRecord::UndoRecoveryUpdate { state: cid(17) },
        ];

        for expected in same_shape_records {
            let bytes = tests_support::encode_atomic_no_head(&expected).unwrap();
            let decoded = CurrentOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }

    #[test]
    fn pre_atomic_schema_maps_every_supported_variant() {
        for expected in pre_atomic_supported_records() {
            let bytes = tests_support::encode_pre_atomic(&expected).unwrap();
            let decoded = PreAtomicOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }

    #[test]
    fn atomic_no_head_schema_maps_every_variant() {
        for expected in atomic_no_head_records() {
            let bytes = tests_support::encode_atomic_no_head(&expected).unwrap();
            let decoded = AtomicNoHeadOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }

    #[test]
    fn current_schema_round_trips_visibility_variants() {
        // heddle#317 tail variants only exist in the current schema; prove
        // they survive an encode → current-decode round-trip so the audit
        // entries are readable back.
        let records = [
            OpRecord::StateVisibilitySet {
                state: cid(30),
                record_id: hash(31),
                tier: VisibilityTier::Restricted {
                    scope_label: "embargo".into(),
                },
                prior_sidecar: None,
                new_sidecar: Some(vec![1, 2, 3, 4]),
            },
            OpRecord::StateVisibilityPromote {
                state: cid(32),
                superseded: hash(33),
                record_id: hash(34),
                tier: VisibilityTier::Internal,
                prior_sidecar: Some(vec![5, 6, 7]),
                new_sidecar: Some(vec![8, 9, 10]),
            },
        ];
        for expected in records {
            let bytes = encode_latest_record(&expected).unwrap();
            let decoded = CurrentOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
            let decoded = decode_versioned_record(&bytes, OpRecordSchemaVersion::Current).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }

    #[test]
    fn current_schema_round_trips_thread_update_manager_snapshots() {
        let records = [
            OpRecord::ThreadUpdate {
                name: "main".into(),
                old_state: cid(6),
                new_state: cid(7),
                manager_snapshots: ThreadUpdateSnapshots::from_parts(Some(vec![6]), Some(vec![7])),
            },
            OpRecord::ThreadUpdate {
                name: "main".into(),
                old_state: cid(6),
                new_state: cid(7),
                manager_snapshots: ThreadUpdateSnapshots::from_parts(None, Some(vec![7])),
            },
        ];
        for expected in records {
            let bytes = encode_latest_record(&expected).unwrap();
            let decoded = CurrentOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
            let decoded = decode_versioned_record(&bytes, OpRecordSchemaVersion::Current).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }

    #[test]
    fn thread_update_without_snapshots_keeps_legacy_bytes() {
        let expected = OpRecord::ThreadUpdate {
            name: "main".into(),
            old_state: cid(6),
            new_state: cid(7),
            manager_snapshots: None,
        };
        let bytes = encode_latest_record(&expected).unwrap();
        let legacy_bytes = tests_support::encode_atomic_no_head(&expected).unwrap();

        assert_eq!(bytes, legacy_bytes);
        assert_eq!(
            bytes,
            vec![
                129, 172, 84, 104, 114, 101, 97, 100, 85, 112, 100, 97, 116, 101, 147, 164, 109,
                97, 105, 110, 220, 0, 16, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 220, 0,
                16, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
            ]
        );
        let decoded = AtomicNoHeadOpRecordSchema::decode(&bytes).unwrap();
        assert_same_record(&decoded, &expected);
    }

    #[test]
    fn old_thread_update_reader_refuses_snapshot_tail() {
        let expected = OpRecord::ThreadUpdate {
            name: "main".into(),
            old_state: cid(6),
            new_state: cid(7),
            manager_snapshots: ThreadUpdateSnapshots::from_parts(None, Some(vec![7])),
        };
        let bytes = encode_latest_record(&expected).unwrap();
        let error = rmp_serde::from_slice::<AtomicNoHeadOpRecord>(&bytes).unwrap_err();
        assert!(
            format!("{error:?}").contains("LengthMismatch"),
            "expected positional length refusal, got {error}"
        );
    }

    #[test]
    fn current_schema_round_trips_through_versioned_decoder() {
        for expected in atomic_no_head_records() {
            let bytes = encode_latest_record(&expected).unwrap();
            let decoded = CurrentOpRecordSchema::decode(&bytes).unwrap();
            assert_same_record(&decoded, &expected);
            let decoded = decode_versioned_record(&bytes, OpRecordSchemaVersion::Current).unwrap();
            assert_same_record(&decoded, &expected);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub(crate) fn encode_pre_atomic(record: &OpRecord) -> Result<Vec<u8>> {
        let legacy = PreAtomicOpRecord::from_current_fixture(record);
        rmp_serde::to_vec(&legacy).map_err(|e| HeddleError::Serialization(e.to_string()))
    }

    pub(crate) fn encode_atomic_no_head(record: &OpRecord) -> Result<Vec<u8>> {
        let legacy = AtomicNoHeadOpRecord::from_current_fixture(record);
        rmp_serde::to_vec(&legacy).map_err(|e| HeddleError::Serialization(e.to_string()))
    }

    impl PreAtomicOpRecord {
        fn from_current_fixture(record: &OpRecord) -> Self {
            match record {
                OpRecord::Snapshot {
                    new_state,
                    prev_head,
                    thread,
                    ..
                } => Self::Snapshot {
                    new_state: *new_state,
                    prev_head: *prev_head,
                    thread: thread.clone(),
                },
                OpRecord::Goto {
                    target, prev_head, ..
                } => Self::Goto {
                    target: *target,
                    prev_head: *prev_head,
                },
                OpRecord::ThreadCreate {
                    name,
                    state,
                    manager_snapshot,
                } => Self::ThreadCreate {
                    name: name.clone(),
                    state: *state,
                    manager_snapshot: manager_snapshot.clone(),
                },
                OpRecord::ThreadDelete { name, state } => Self::ThreadDelete {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::ThreadUpdate {
                    name,
                    old_state,
                    new_state,
                    ..
                } => Self::ThreadUpdate {
                    name: name.clone(),
                    old_state: *old_state,
                    new_state: *new_state,
                },
                OpRecord::Fork {
                    from, new_state, ..
                } => {
                    // Fixtures model the historical CLI bytes, which stored
                    // these two fields reversed.
                    Self::Fork {
                        from: *new_state,
                        new_state: *from,
                    }
                }
                OpRecord::Collapse {
                    sources, result, ..
                } => Self::Collapse {
                    sources: sources.clone(),
                    result: *result,
                },
                OpRecord::MarkerCreate { name, state } => Self::MarkerCreate {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::MarkerDelete { name, state } => Self::MarkerDelete {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::Checkpoint {
                    parent,
                    state,
                    thread,
                } => Self::Checkpoint {
                    parent: *parent,
                    state: *state,
                    thread: thread.clone(),
                },
                OpRecord::TransactionAbort {
                    transaction_id,
                    reason,
                } => Self::TransactionAbort {
                    transaction_id: transaction_id.clone(),
                    reason: reason.clone(),
                },
                OpRecord::EphemeralThreadCollapse {
                    thread,
                    final_state,
                } => Self::EphemeralThreadCollapse {
                    thread: thread.clone(),
                    final_state: *final_state,
                },
                OpRecord::ConflictResolved {
                    conflict_id,
                    resolution,
                } => Self::ConflictResolved {
                    conflict_id: conflict_id.clone(),
                    resolution: resolution.clone(),
                },
                OpRecord::TransactionCommit {
                    transaction_id,
                    op_count,
                } => Self::TransactionCommit {
                    transaction_id: transaction_id.clone(),
                    op_count: *op_count,
                },
                OpRecord::Redact {
                    redaction_id,
                    blob,
                    state,
                    path,
                } => Self::Redact {
                    redaction_id: *redaction_id,
                    blob: *blob,
                    state: *state,
                    path: path.clone(),
                },
                OpRecord::Purge { redaction_id, blob } => Self::Purge {
                    redaction_id: *redaction_id,
                    blob: *blob,
                },
                OpRecord::FastForward {
                    source_thread,
                    target_thread,
                    pre_target_id,
                    post_target_id,
                } => Self::FastForward {
                    source_thread: source_thread.clone(),
                    target_thread: target_thread.clone(),
                    pre_target_id: *pre_target_id,
                    post_target_id: *post_target_id,
                },
                OpRecord::GitCheckpoint {
                    branch,
                    state,
                    previous_git_oid,
                    new_git_oid,
                } => Self::GitCheckpoint {
                    branch: branch.clone(),
                    state: *state,
                    previous_git_oid: previous_git_oid.clone(),
                    new_git_oid: new_git_oid.clone(),
                },
                OpRecord::RemoteThreadUpdate { .. }
                | OpRecord::RemoteThreadDelete { .. }
                | OpRecord::UndoRecoveryUpdate { .. }
                | OpRecord::StateVisibilitySet { .. }
                | OpRecord::StateVisibilityPromote { .. } => {
                    panic!("pre-atomic fixtures cannot encode post-atomic tail variants")
                }
            }
        }
    }

    impl AtomicNoHeadOpRecord {
        fn from_current_fixture(record: &OpRecord) -> Self {
            match record {
                OpRecord::Snapshot {
                    new_state,
                    prev_head,
                    thread,
                    ..
                } => Self::Snapshot {
                    new_state: *new_state,
                    prev_head: *prev_head,
                    thread: thread.clone(),
                },
                OpRecord::Goto {
                    target, prev_head, ..
                } => Self::Goto {
                    target: *target,
                    prev_head: *prev_head,
                },
                OpRecord::ThreadCreate {
                    name,
                    state,
                    manager_snapshot,
                } => Self::ThreadCreate {
                    name: name.clone(),
                    state: *state,
                    manager_snapshot: manager_snapshot.clone(),
                },
                OpRecord::ThreadDelete { name, state } => Self::ThreadDelete {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::ThreadUpdate {
                    name,
                    old_state,
                    new_state,
                    ..
                } => Self::ThreadUpdate {
                    name: name.clone(),
                    old_state: *old_state,
                    new_state: *new_state,
                },
                OpRecord::Fork {
                    from,
                    new_state,
                    thread,
                    head,
                } => Self::Fork {
                    from: *from,
                    new_state: *new_state,
                    thread: thread.clone(),
                    head: *head,
                },
                OpRecord::Collapse {
                    sources,
                    result,
                    thread,
                } => Self::Collapse {
                    sources: sources.clone(),
                    result: *result,
                    thread: thread.clone(),
                },
                OpRecord::MarkerCreate { name, state } => Self::MarkerCreate {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::MarkerDelete { name, state } => Self::MarkerDelete {
                    name: name.clone(),
                    state: *state,
                },
                OpRecord::Checkpoint {
                    parent,
                    state,
                    thread,
                } => Self::Checkpoint {
                    parent: *parent,
                    state: *state,
                    thread: thread.clone(),
                },
                OpRecord::TransactionAbort {
                    transaction_id,
                    reason,
                } => Self::TransactionAbort {
                    transaction_id: transaction_id.clone(),
                    reason: reason.clone(),
                },
                OpRecord::EphemeralThreadCollapse {
                    thread,
                    final_state,
                } => Self::EphemeralThreadCollapse {
                    thread: thread.clone(),
                    final_state: *final_state,
                },
                OpRecord::ConflictResolved {
                    conflict_id,
                    resolution,
                } => Self::ConflictResolved {
                    conflict_id: conflict_id.clone(),
                    resolution: resolution.clone(),
                },
                OpRecord::TransactionCommit {
                    transaction_id,
                    op_count,
                } => Self::TransactionCommit {
                    transaction_id: transaction_id.clone(),
                    op_count: *op_count,
                },
                OpRecord::Redact {
                    redaction_id,
                    blob,
                    state,
                    path,
                } => Self::Redact {
                    redaction_id: *redaction_id,
                    blob: *blob,
                    state: *state,
                    path: path.clone(),
                },
                OpRecord::Purge { redaction_id, blob } => Self::Purge {
                    redaction_id: *redaction_id,
                    blob: *blob,
                },
                OpRecord::FastForward {
                    source_thread,
                    target_thread,
                    pre_target_id,
                    post_target_id,
                } => Self::FastForward {
                    source_thread: source_thread.clone(),
                    target_thread: target_thread.clone(),
                    pre_target_id: *pre_target_id,
                    post_target_id: *post_target_id,
                },
                OpRecord::GitCheckpoint {
                    branch,
                    state,
                    previous_git_oid,
                    new_git_oid,
                } => Self::GitCheckpoint {
                    branch: branch.clone(),
                    state: *state,
                    previous_git_oid: previous_git_oid.clone(),
                    new_git_oid: new_git_oid.clone(),
                },
                OpRecord::RemoteThreadUpdate {
                    remote,
                    thread,
                    state,
                } => Self::RemoteThreadUpdate {
                    remote: remote.clone(),
                    thread: thread.clone(),
                    state: *state,
                },
                OpRecord::RemoteThreadDelete {
                    remote,
                    thread,
                    state,
                } => Self::RemoteThreadDelete {
                    remote: remote.clone(),
                    thread: thread.clone(),
                    state: *state,
                },
                OpRecord::UndoRecoveryUpdate { state } => {
                    Self::UndoRecoveryUpdate { state: *state }
                }
                OpRecord::StateVisibilitySet { .. } | OpRecord::StateVisibilityPromote { .. } => {
                    panic!("atomic-no-head fixtures cannot encode heddle#317 visibility variants")
                }
            }
        }
    }
}
