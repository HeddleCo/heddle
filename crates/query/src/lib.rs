// SPDX-License-Identifier: Apache-2.0
//! Operation-log query engine shared by the CLI and local daemon RPC adapter.
//!
//! The core query path is intentionally transport-free: callers provide a
//! Heddle directory and an [`OperationLogQuery`], and this crate combines the
//! rebuildable operation index with a bounded live-oplog fallback.

use std::{collections::BTreeMap, path::Path};

use objects::{error::Result as HeddleResult, object::ChangeId};
use oplog::{BlockingOpLogBackend, OpEntry, OpLog, OpRecord};
pub use refs::operation_index::{IndexedOperation, OperationLogIndex, OperationLogQuery};

/// Query is an operator-facing inspection command, so it should answer from
/// the live oplog even before the rebuildable index sidecar has been warmed.
/// Keep the scan bounded; long-tail history can use the index once populated.
pub const OPLOG_FALLBACK_SCAN_WINDOW: usize = 100_000;

/// Apply the default user-facing checkpoint filter to a query.
///
/// Derived from the oplog verb catalog, not a hand-maintained list, so a new
/// [`OpRecord`] variant joins the default query surface as soon as it enters
/// the catalog.
pub fn apply_checkpoint_filter(query: &mut OperationLogQuery, include_checkpoints: bool) {
    if !include_checkpoints && query.verbs.is_none() {
        query.verbs = Some(default_verbs(include_checkpoints));
    }
}

pub fn default_verbs(include_checkpoints: bool) -> Vec<String> {
    OpRecord::verbs(include_checkpoints)
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub fn query_operations(
    heddle_dir: impl AsRef<Path>,
    query: &OperationLogQuery,
) -> HeddleResult<Vec<IndexedOperation>> {
    let heddle_dir = heddle_dir.as_ref();
    let index = OperationLogIndex::new(heddle_dir);
    let mut unbounded = query.clone();
    unbounded.limit = None;

    let mut by_seq = BTreeMap::new();
    for hit in index.query(&unbounded)? {
        by_seq.insert(hit.seq, hit);
    }

    if unbounded.symbol.is_none() && unbounded.signal_kind.is_none() {
        for hit in query_oplog_fallback(heddle_dir, &unbounded)? {
            by_seq.entry(hit.seq).or_insert(hit);
        }
    }

    let mut hits: Vec<_> = by_seq.into_values().collect();
    hits.sort_by_key(|hit| hit.seq);
    if let Some(limit) = query.limit {
        hits.truncate(limit);
    }
    Ok(hits)
}

fn query_oplog_fallback(
    heddle_dir: &Path,
    query: &OperationLogQuery,
) -> HeddleResult<Vec<IndexedOperation>> {
    let log = OpLog::new_unattributed(heddle_dir);
    let mut entries = log.recent(OPLOG_FALLBACK_SCAN_WINDOW)?;
    entries.reverse();
    let mut hits = Vec::new();
    for entry in entries {
        let hit = indexed_from_oplog_entry(&entry);
        if indexed_operation_matches(&hit, query) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

pub fn indexed_from_oplog_entry(entry: &OpEntry) -> IndexedOperation {
    IndexedOperation {
        seq: entry.id,
        timestamp_secs: entry.timestamp.timestamp(),
        verb: entry.operation.verb().to_string(),
        actor_email: entry.actor.email.clone(),
        operation_id: entry.operation_id,
        thread: thread_for(&entry.operation),
        symbols: Vec::new(),
        signal_kinds: Vec::new(),
        change_id: primary_change_id(&entry.operation),
    }
}

pub fn indexed_operation_matches(hit: &IndexedOperation, query: &OperationLogQuery) -> bool {
    if let Some(actor) = &query.actor
        && &hit.actor_email != actor
    {
        return false;
    }
    if let Some(symbol) = &query.symbol
        && !hit.symbols.iter().any(|candidate| candidate == symbol)
    {
        return false;
    }
    if let Some(kind) = &query.signal_kind
        && !hit.signal_kinds.iter().any(|candidate| candidate == kind)
    {
        return false;
    }
    if let Some(thread) = &query.thread
        && hit.thread.as_deref() != Some(thread.as_str())
    {
        return false;
    }
    if let Some(verbs) = &query.verbs
        && !verbs.iter().any(|verb| verb == &hit.verb)
    {
        return false;
    }
    let timestamp = hit.timestamp();
    if let Some(start) = query.since
        && timestamp < start
    {
        return false;
    }
    if let Some(end) = query.until
        && timestamp > end
    {
        return false;
    }
    true
}

fn thread_for(op: &OpRecord) -> Option<String> {
    match op {
        OpRecord::Snapshot { thread, .. } => thread.clone(),
        OpRecord::ThreadCreate { name, .. } => Some(name.clone()),
        OpRecord::ThreadDelete { name, .. } => Some(name.clone()),
        OpRecord::ThreadUpdate { name, .. } => Some(name.clone()),
        OpRecord::MarkerCreate { name, .. } => Some(name.clone()),
        OpRecord::MarkerDelete { name, .. } => Some(name.clone()),
        OpRecord::Checkpoint { thread, .. } => thread.clone(),
        OpRecord::EphemeralThreadCollapse { thread, .. } => Some(thread.clone()),
        OpRecord::FastForward { target_thread, .. } => Some(target_thread.clone()),
        OpRecord::GitCheckpoint { branch, .. } => Some(branch.clone()),
        OpRecord::RemoteThreadUpdate { thread, .. }
        | OpRecord::RemoteThreadDelete { thread, .. } => Some(thread.clone()),
        OpRecord::Goto { .. }
        | OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Redact { .. }
        | OpRecord::UndoRecoveryUpdate { .. }
        | OpRecord::StateVisibilitySet { .. }
        | OpRecord::StateVisibilityPromote { .. }
        | OpRecord::Purge { .. } => None,
    }
}

fn primary_change_id(op: &OpRecord) -> Option<ChangeId> {
    match op {
        OpRecord::Snapshot { new_state, .. } => Some(*new_state),
        OpRecord::Goto { target, .. } => Some(*target),
        OpRecord::ThreadCreate { state, .. } => Some(*state),
        OpRecord::ThreadDelete { state, .. } => Some(*state),
        OpRecord::ThreadUpdate { new_state, .. } => Some(*new_state),
        OpRecord::Fork { new_state, .. } => Some(*new_state),
        OpRecord::Collapse { result, .. } => Some(*result),
        OpRecord::MarkerCreate { state, .. } => Some(*state),
        OpRecord::MarkerDelete { state, .. } => Some(*state),
        OpRecord::Checkpoint { state, .. } => Some(*state),
        OpRecord::GitCheckpoint { state, .. } => Some(*state),
        OpRecord::EphemeralThreadCollapse { final_state, .. } => Some(*final_state),
        OpRecord::Redact { state, .. } => Some(*state),
        OpRecord::StateVisibilitySet { state, .. }
        | OpRecord::StateVisibilityPromote { state, .. } => Some(*state),
        OpRecord::RemoteThreadUpdate { state, .. } | OpRecord::RemoteThreadDelete { state, .. } => {
            Some(*state)
        }
        OpRecord::UndoRecoveryUpdate { state } => Some(*state),
        OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Purge { .. }
        | OpRecord::FastForward { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use objects::object::OperationId;
    use oplog::{OpLog, OpRecord};
    use tempfile::TempDir;

    use super::*;

    fn make_op(seq: u64, ts_secs: i64, actor: &str, verb: &str) -> IndexedOperation {
        IndexedOperation {
            seq,
            timestamp_secs: ts_secs,
            verb: verb.to_string(),
            actor_email: actor.to_string(),
            operation_id: None,
            thread: Some("main".into()),
            symbols: vec!["src/lib.rs:foo".into()],
            signal_kinds: vec![],
            change_id: Some(ChangeId::from_bytes([1; 16])),
        }
    }

    #[test]
    fn query_combines_index_and_live_oplog_fallback() {
        let temp = TempDir::new().unwrap();
        let index = OperationLogIndex::new(temp.path());
        index
            .record(make_op(100, 1_700_000_000, "alice@example.com", "snapshot"))
            .unwrap();

        let log = OpLog::new_unattributed(temp.path());
        log.record_batch(vec![OpRecord::ThreadCreate {
            name: "feature".into(),
            state: ChangeId::from_bytes([2; 16]),
            manager_snapshot: None,
        }])
        .unwrap();

        let hits = query_operations(
            temp.path(),
            &OperationLogQuery {
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|hit| hit.actor_email == "alice@example.com")
        );
        assert!(hits.iter().any(|hit| hit.verb == "thread_create"));
    }

    #[test]
    fn query_filters_live_oplog_fallback() {
        let temp = TempDir::new().unwrap();
        let log = OpLog::new_unattributed(temp.path());
        log.record_batch(vec![OpRecord::ThreadCreate {
            name: "feature".into(),
            state: ChangeId::from_bytes([3; 16]),
            manager_snapshot: None,
        }])
        .unwrap();

        let hits = query_operations(
            temp.path(),
            &OperationLogQuery {
                actor: Some("nobody@example.com".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(hits.is_empty());
    }

    #[test]
    fn indexed_match_respects_time_window() {
        let hit = make_op(1, 1_700_000_000, "alice@example.com", "snapshot");
        assert!(indexed_operation_matches(
            &hit,
            &OperationLogQuery {
                since: Utc.timestamp_opt(1_699_999_999, 0).single(),
                until: Utc.timestamp_opt(1_700_000_001, 0).single(),
                ..Default::default()
            },
        ));
        assert!(!indexed_operation_matches(
            &hit,
            &OperationLogQuery {
                since: Utc.timestamp_opt(1_700_000_001, 0).single(),
                ..Default::default()
            },
        ));
    }

    #[test]
    fn checkpoint_filter_populates_default_verbs() {
        let mut query = OperationLogQuery::default();
        apply_checkpoint_filter(&mut query, false);
        let verbs = query.verbs.expect("default verbs");
        assert!(!verbs.iter().any(|verb| verb == "checkpoint"));
    }

    #[test]
    fn checkpoint_filter_respects_explicit_verbs() {
        let mut query = OperationLogQuery {
            verbs: Some(vec!["checkpoint".into()]),
            ..Default::default()
        };
        apply_checkpoint_filter(&mut query, false);
        assert_eq!(query.verbs, Some(vec!["checkpoint".into()]));
    }

    #[test]
    fn operation_id_type_stays_in_index_surface() {
        let mut hit = make_op(1, 1_700_000_000, "alice@example.com", "snapshot");
        hit.operation_id = Some(OperationId::new());
        assert!(hit.operation_id.is_some());
    }
}
