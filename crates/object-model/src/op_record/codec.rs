// SPDX-License-Identifier: Apache-2.0
//! Versioned codec for current OpRecord payloads stored in the packed oplog.
//!
//! Repository format v3 changed state identity from 16-byte logical ChangeIds
//! to 32-byte content-addressed StateIds. That boundary is intentionally not
//! migratable: repository open refuses older formats, and the oplog codec
//! refuses record schema versions 1 through 3 without rewriting their bytes.
//! Future incompatible record changes must allocate a new schema version.

use serde::Deserialize;

use super::OpRecord;
use crate::error::{HeddleError, Result};

pub const CURRENT_OP_RECORD_SCHEMA_VERSION: u32 = 4;
const CURRENT_OP_RECORD_SCHEMA_NAME: &str = "state-id-v4";
const OP_RECORD_STORAGE: &str = "oplog record schema";

pub fn validate_op_record_schema_version(version: u32) -> Result<()> {
    if version < CURRENT_OP_RECORD_SCHEMA_VERSION {
        return Err(HeddleError::StorageFormatTooOld {
            storage: OP_RECORD_STORAGE.to_string(),
            found: version,
            required: CURRENT_OP_RECORD_SCHEMA_VERSION,
        });
    }
    if version > CURRENT_OP_RECORD_SCHEMA_VERSION {
        return Err(HeddleError::StorageFormatTooNew {
            storage: OP_RECORD_STORAGE.to_string(),
            found: version,
            supported: CURRENT_OP_RECORD_SCHEMA_VERSION,
        });
    }
    Ok(())
}

pub fn decode_current_record(bytes: &[u8]) -> Result<OpRecord> {
    decode_rmp(bytes, CURRENT_OP_RECORD_SCHEMA_NAME)
}

pub fn encode_current_record(record: &OpRecord) -> Result<Vec<u8>> {
    rmp_serde::to_vec(record).map_err(|e| HeddleError::Serialization(e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::super::{ConflictResolutionMode, RecordedHead, ThreadUpdateSnapshots};
    use super::*;
    use crate::object::{Agent, Attribution, ContentHash, Principal, StateId, VisibilityTier};

    fn state(byte: u8) -> StateId {
        StateId::from_bytes([byte; 32])
    }

    fn hash(byte: u8) -> ContentHash {
        ContentHash::from_bytes([byte; 32])
    }

    fn assert_round_trip(record: OpRecord) {
        let bytes = encode_current_record(&record).unwrap();
        let decoded = decode_current_record(&bytes).unwrap();
        assert_eq!(format!("{decoded:?}"), format!("{record:?}"));
    }

    fn canonical_current_records() -> Vec<OpRecord> {
        vec![
            OpRecord::Snapshot {
                new_state: state(1),
                prev_head: Some(state(2)),
                head: None,
                thread: Some("main".into()),
            },
            OpRecord::Goto {
                target: state(3),
                prev_head: Some(state(2)),
                head: state(3),
            },
            OpRecord::ThreadCreate {
                name: "topic".into(),
                state: state(4),
                manager_snapshot: Some(vec![1, 2, 3]),
            },
            OpRecord::ThreadDelete {
                name: "old".into(),
                state: state(5),
            },
            OpRecord::ThreadUpdate {
                name: "main".into(),
                old_state: state(6),
                new_state: state(7),
                manager_snapshots: ThreadUpdateSnapshots::from_record_sets(
                    Some(vec![6]),
                    Some(vec![7]),
                    vec![vec![60], vec![61]],
                    vec![vec![70]],
                    true,
                ),
            },
            OpRecord::Fork {
                from: state(8),
                new_state: state(9),
                thread: Some("topic".into()),
                head: None,
            },
            OpRecord::Collapse {
                sources: vec![state(8), state(9)],
                result: state(10),
                thread: Some("main".into()),
                pre_thread_state: Some(state(7)),
            },
            OpRecord::MarkerCreate {
                name: "release".into(),
                state: state(11),
            },
            OpRecord::MarkerDelete {
                name: "draft".into(),
                state: state(12),
            },
            OpRecord::Checkpoint {
                parent: Some(state(12)),
                state: state(13),
                thread: Some("main".into()),
            },
            OpRecord::TransactionAbort {
                transaction_id: "abort".into(),
                reason: "reason".into(),
            },
            OpRecord::EphemeralThreadCollapse {
                thread: "ephemeral".into(),
                final_state: state(14),
            },
            OpRecord::ConflictResolved {
                conflict_id: "conflict".into(),
                resolution: "ours".into(),
                resolver: Attribution::with_agent(
                    Principal::new("Resolver", "resolver@example.com"),
                    Agent::new("openai", "gpt-5-codex"),
                ),
                mode: ConflictResolutionMode::Ours,
            },
            OpRecord::TransactionCommit {
                transaction_id: "tx".into(),
                op_count: 2,
            },
            OpRecord::Redact {
                redaction_id: hash(1),
                blob: hash(2),
                state: state(15),
                path: "secret.txt".into(),
            },
            OpRecord::Purge {
                redaction_id: hash(3),
                blob: hash(4),
            },
            OpRecord::FastForward {
                source_thread: "feature".into(),
                target_thread: "main".into(),
                pre_target_id: state(17),
                post_target_id: state(18),
            },
            OpRecord::GitCheckpoint {
                branch: "main".into(),
                state: state(20),
                previous_git_oid: Some("abc".into()),
                new_git_oid: "def".into(),
            },
            OpRecord::RemoteThreadUpdate {
                remote: "origin".into(),
                thread: "main".into(),
                state: state(21),
            },
            OpRecord::RemoteThreadDelete {
                remote: "origin".into(),
                thread: "old".into(),
                state: state(22),
            },
            OpRecord::UndoRecoveryUpdate { state: state(23) },
            OpRecord::StateVisibilitySet {
                state: state(24),
                record_id: hash(5),
                tier: VisibilityTier::Internal,
                prior_sidecar: None,
                new_sidecar: Some(vec![1, 2, 3]),
            },
            OpRecord::StateVisibilityPromote {
                state: state(25),
                superseded: hash(6),
                record_id: hash(7),
                tier: VisibilityTier::Restricted {
                    scope_label: "embargo".into(),
                },
                prior_sidecar: Some(vec![4]),
                new_sidecar: Some(vec![5]),
            },
            OpRecord::HeadUpdate {
                previous: RecordedHead::Detached { state: state(26) },
                new: RecordedHead::Attached {
                    thread: "main".into(),
                },
            },
            OpRecord::EntryVisibilitySet {
                change_id: crate::object::ChangeId::from_bytes([7u8; 16]),
                record_id: hash(8),
                prior_sidecar: None,
                new_sidecar: Some(vec![9, 9, 9]),
            },
        ]
    }

    fn variant_name(record: &OpRecord) -> &'static str {
        match record {
            OpRecord::Snapshot { .. } => "Snapshot",
            OpRecord::Goto { .. } => "Goto",
            OpRecord::ThreadCreate { .. } => "ThreadCreate",
            OpRecord::ThreadDelete { .. } => "ThreadDelete",
            OpRecord::ThreadUpdate { .. } => "ThreadUpdate",
            OpRecord::Fork { .. } => "Fork",
            OpRecord::Collapse { .. } => "Collapse",
            OpRecord::MarkerCreate { .. } => "MarkerCreate",
            OpRecord::MarkerDelete { .. } => "MarkerDelete",
            OpRecord::Checkpoint { .. } => "Checkpoint",
            OpRecord::TransactionAbort { .. } => "TransactionAbort",
            OpRecord::EphemeralThreadCollapse { .. } => "EphemeralThreadCollapse",
            OpRecord::ConflictResolved { .. } => "ConflictResolved",
            OpRecord::TransactionCommit { .. } => "TransactionCommit",
            OpRecord::Redact { .. } => "Redact",
            OpRecord::Purge { .. } => "Purge",
            OpRecord::FastForward { .. } => "FastForward",
            OpRecord::GitCheckpoint { .. } => "GitCheckpoint",
            OpRecord::RemoteThreadUpdate { .. } => "RemoteThreadUpdate",
            OpRecord::RemoteThreadDelete { .. } => "RemoteThreadDelete",
            OpRecord::UndoRecoveryUpdate { .. } => "UndoRecoveryUpdate",
            OpRecord::StateVisibilitySet { .. } => "StateVisibilitySet",
            OpRecord::StateVisibilityPromote { .. } => "StateVisibilityPromote",
            OpRecord::HeadUpdate { .. } => "HeadUpdate",
            OpRecord::EntryVisibilitySet { .. } => "EntryVisibilitySet",
        }
    }

    #[test]
    fn schema_four_is_current_and_legacy_versions_are_refused() {
        assert_eq!(CURRENT_OP_RECORD_SCHEMA_VERSION, 4);
        validate_op_record_schema_version(4).unwrap();
        for legacy in 1..=3 {
            let error = validate_op_record_schema_version(legacy).unwrap_err();
            assert!(matches!(
                error,
                HeddleError::StorageFormatTooOld {
                    found,
                    required: 4,
                    ..
                } if found == legacy
            ));
        }
        assert!(matches!(
            validate_op_record_schema_version(5).unwrap_err(),
            HeddleError::StorageFormatTooNew {
                found: 5,
                supported: 4,
                ..
            }
        ));
    }

    #[test]
    fn every_current_variant_round_trips() {
        let records = canonical_current_records();
        assert_eq!(
            records.iter().map(variant_name).collect::<Vec<_>>(),
            [
                "Snapshot",
                "Goto",
                "ThreadCreate",
                "ThreadDelete",
                "ThreadUpdate",
                "Fork",
                "Collapse",
                "MarkerCreate",
                "MarkerDelete",
                "Checkpoint",
                "TransactionAbort",
                "EphemeralThreadCollapse",
                "ConflictResolved",
                "TransactionCommit",
                "Redact",
                "Purge",
                "FastForward",
                "GitCheckpoint",
                "RemoteThreadUpdate",
                "RemoteThreadDelete",
                "UndoRecoveryUpdate",
                "StateVisibilitySet",
                "StateVisibilityPromote",
                "HeadUpdate",
                "EntryVisibilitySet",
            ]
        );
        for record in records {
            assert_round_trip(record);
        }
    }

    #[test]
    fn state_id_v4_visibility_tail_bytes_are_frozen() {
        let record = OpRecord::StateVisibilityPromote {
            state: state(1),
            superseded: hash(2),
            record_id: hash(3),
            tier: VisibilityTier::Internal,
            prior_sidecar: Some(vec![4]),
            new_sidecar: Some(vec![5]),
        };

        let expected = [
            &[
                129, 182, 83, 116, 97, 116, 101, 86, 105, 115, 105, 98, 105, 108, 105, 116, 121,
                80, 114, 111, 109, 111, 116, 101, 150, 220, 0, 32,
            ][..],
            &[1; 32],
            &[220, 0, 32],
            &[2; 32],
            &[220, 0, 32],
            &[3; 32],
            &[168, 73, 110, 116, 101, 114, 110, 97, 108, 145, 4, 145, 5],
        ]
        .concat();

        assert_eq!(encode_current_record(&record).unwrap(), expected);
    }

    #[test]
    fn historical_sixteen_byte_payload_is_not_a_state_id_record() {
        let historical = [
            129, 168, 67, 111, 108, 108, 97, 112, 115, 101, 147, 146, 220, 0, 16, 10, 10, 10, 10,
            10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 220, 0, 16, 11, 11, 11, 11, 11, 11, 11,
            11, 11, 11, 11, 11, 11, 11, 11, 11, 220, 0, 16, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
            12, 12, 12, 12, 12, 12, 164, 109, 97, 105, 110,
        ];
        let error = decode_current_record(&historical)
            .expect_err("16-byte ChangeIds must not decode as StateIds");
        assert!(error.to_string().contains("expected an array of length 32"));
    }
}
