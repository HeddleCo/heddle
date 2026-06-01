// SPDX-License-Identifier: Apache-2.0
//! Local gRPC service for the W2 `OperationLogQueryService`.
//!
//! Read-only wrapper around [`refs::operation_index::OperationLogIndex`].
//! Translates protobuf request/response shapes to/from the index's native
//! [`OperationLogQuery`] / [`IndexedOperation`] types. No state changes,
//! no idempotency wrapper.

use std::{collections::BTreeMap, path::Path, pin::Pin};

use chrono::TimeZone;
use futures::Stream;
use grpc::heddle::v1::{
    OperationHit, QueryOperationsRequest, QueryOperationsResponse, StreamOperationsRequest,
    operation_log_query_service_server::OperationLogQueryService,
};
use objects::{error::Result as HeddleResult, object::ChangeId};
use oplog::{OpEntry, OpLog, OpRecord};
use refs::operation_index::{IndexedOperation, OperationLogIndex, OperationLogQuery};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status};

#[derive(Clone)]
pub struct LocalOperationLogQueryService {
    inner: GrpcLocalService,
}

impl LocalOperationLogQueryService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

/// Convert a unix-epoch-seconds field from the proto. `0` means "unset"
/// because proto3 scalars don't distinguish presence; any other value is
/// passed to [`chrono::Utc.timestamp_opt`] and discarded if out of range.
fn parse_unix_secs(secs: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    if secs == 0 {
        return None;
    }
    chrono::Utc.timestamp_opt(secs, 0).single()
}

/// Query is an operator-facing inspection command, so it should answer from
/// the live oplog even before the rebuildable index sidecar has been warmed.
/// Keep the scan bounded; long-tail history can use the index once populated.
const OPLOG_FALLBACK_SCAN_WINDOW: usize = 100_000;

fn build_query(req: &QueryOperationsRequest) -> OperationLogQuery {
    let mut q = OperationLogQuery {
        actor: (!req.actor.is_empty()).then(|| req.actor.clone()),
        symbol: (!req.symbol.is_empty()).then(|| req.symbol.clone()),
        signal_kind: (!req.signal_kind.is_empty()).then(|| req.signal_kind.clone()),
        thread: (!req.thread.is_empty()).then(|| req.thread.clone()),
        verbs: (!req.verbs.is_empty()).then(|| req.verbs.clone()),
        since: parse_unix_secs(req.since_secs),
        until: parse_unix_secs(req.until_secs),
        limit: (req.limit > 0).then_some(req.limit as usize),
    };
    if !req.include_checkpoints && q.verbs.is_none() {
        // Derived from the oplog verb catalog (the single source of truth), not
        // a hand-maintained list — so a new `OpRecord` variant is surfaced by
        // default the moment it joins the catalog, instead of being silently
        // dropped from the default query (heddle#354 r9, cid 3330304663).
        q.verbs = Some(OpRecord::verbs(false).iter().map(|s| s.to_string()).collect());
    }
    q
}

fn hit_to_proto(hit: IndexedOperation) -> OperationHit {
    OperationHit {
        seq: hit.seq,
        timestamp_secs: hit.timestamp_secs,
        verb: hit.verb,
        actor_email: hit.actor_email,
        operation_id: hit.operation_id.map(|o| o.to_string()).unwrap_or_default(),
        thread: hit.thread.unwrap_or_default(),
        symbols: hit.symbols,
        signal_kinds: hit.signal_kinds,
        change_id: hit
            .change_id
            .map(|c| c.as_bytes().to_vec())
            .unwrap_or_default(),
    }
}

fn query_combined(
    heddle_dir: &Path,
    query: &OperationLogQuery,
) -> HeddleResult<Vec<IndexedOperation>> {
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

fn indexed_from_oplog_entry(entry: &OpEntry) -> IndexedOperation {
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

fn indexed_operation_matches(hit: &IndexedOperation, query: &OperationLogQuery) -> bool {
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
        OpRecord::ThreadCreate { name, .. } | OpRecord::ThreadCreateV2 { name, .. } => {
            Some(name.clone())
        }
        OpRecord::ThreadDelete { name, .. } => Some(name.clone()),
        OpRecord::ThreadUpdate { name, .. } => Some(name.clone()),
        OpRecord::MarkerCreate { name, .. } => Some(name.clone()),
        OpRecord::MarkerDelete { name, .. } => Some(name.clone()),
        OpRecord::Checkpoint { thread, .. } => thread.clone(),
        OpRecord::EphemeralThreadCollapse { thread, .. } => Some(thread.clone()),
        OpRecord::FastForward { target_thread, .. }
        | OpRecord::FastForwardV2 { target_thread, .. } => Some(target_thread.clone()),
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
        | OpRecord::Purge { .. } => None,
    }
}

fn primary_change_id(op: &OpRecord) -> Option<ChangeId> {
    match op {
        OpRecord::Snapshot { new_state, .. } => Some(*new_state),
        OpRecord::Goto { target, .. } => Some(*target),
        OpRecord::ThreadCreate { state, .. } | OpRecord::ThreadCreateV2 { state, .. } => {
            Some(*state)
        }
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
        OpRecord::RemoteThreadUpdate { state, .. }
        | OpRecord::RemoteThreadDelete { state, .. } => Some(*state),
        OpRecord::UndoRecoveryUpdate { state } => Some(*state),
        OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::Purge { .. }
        | OpRecord::FastForward { .. }
        | OpRecord::FastForwardV2 { .. } => None,
    }
}

#[tonic::async_trait]
impl OperationLogQueryService for LocalOperationLogQueryService {
    type StreamOperationsStream = Pin<Box<dyn Stream<Item = Result<OperationHit, Status>> + Send>>;

    async fn query_operations(
        &self,
        request: Request<QueryOperationsRequest>,
    ) -> Result<Response<QueryOperationsResponse>, Status> {
        let req = request.into_inner();
        let q = build_query(&req);
        let hits = query_combined(self.inner.repo().heddle_dir(), &q).map_err(to_status)?;
        let proto_hits = hits.into_iter().map(hit_to_proto).collect();
        Ok(Response::new(QueryOperationsResponse { hits: proto_hits }))
    }

    async fn stream_operations(
        &self,
        request: Request<StreamOperationsRequest>,
    ) -> Result<Response<Self::StreamOperationsStream>, Status> {
        let req = request.into_inner();
        let inner_req = req.query.unwrap_or_default();
        let q = build_query(&inner_req);
        let hits = query_combined(self.inner.repo().heddle_dir(), &q).map_err(to_status)?;

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            for hit in hits {
                if tx.send(Ok(hit_to_proto(hit))).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use objects::object::ChangeId;
    use refs::operation_index::IndexedOperation;
    use repo::{Repository, operation_dedup::OperationDedupStore};
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

    fn fresh_service() -> (TempDir, LocalOperationLogQueryService) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let dedup = OperationDedupStore::open(repo.heddle_dir()).unwrap();
        let inner = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
        let svc = LocalOperationLogQueryService::new(inner);
        (temp, svc)
    }

    fn write_op(svc: &LocalOperationLogQueryService, op: IndexedOperation) {
        let index = OperationLogIndex::new(svc.inner.repo().heddle_dir());
        index.record(op).unwrap();
    }

    fn write_oplog_record(svc: &LocalOperationLogQueryService, op: OpRecord) {
        let log = OpLog::new_unattributed(svc.inner.repo().heddle_dir());
        log.record_batch(vec![op]).unwrap();
    }

    #[tokio::test]
    async fn query_returns_records_within_window() {
        let (_t, svc) = fresh_service();
        write_op(
            &svc,
            make_op(1, 1_700_000_000, "alice@example.com", "snapshot"),
        );
        write_op(
            &svc,
            make_op(2, 1_700_000_060, "bob@example.com", "snapshot"),
        );
        write_op(
            &svc,
            make_op(3, 1_700_000_120, "carol@example.com", "snapshot"),
        );

        let req = QueryOperationsRequest {
            actor: "alice@example.com".into(),
            include_checkpoints: true,
            ..Default::default()
        };
        let resp = svc
            .query_operations(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].seq, 1);
        assert_eq!(resp.hits[0].actor_email, "alice@example.com");
    }

    #[tokio::test]
    async fn query_excludes_checkpoints_by_default_when_verbs_unset() {
        let (_t, svc) = fresh_service();
        write_op(
            &svc,
            make_op(1, 1_700_000_000, "alice@example.com", "checkpoint"),
        );
        write_op(
            &svc,
            make_op(2, 1_700_000_060, "alice@example.com", "snapshot"),
        );

        let req = QueryOperationsRequest {
            include_checkpoints: false,
            ..Default::default()
        };
        let resp = svc
            .query_operations(Request::new(req))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].verb, "snapshot");
    }

    #[tokio::test]
    async fn default_query_includes_newer_non_checkpoint_verbs() {
        // Non-vacuous for cid 3330304663: `transaction_commit` was missing from
        // the old hand-maintained default list, so it was silently dropped from
        // the default (non-checkpoint) view. Now the default is derived from the
        // oplog catalog, so it surfaces.
        let (_t, svc) = fresh_service();
        write_oplog_record(
            &svc,
            OpRecord::TransactionCommit {
                transaction_id: "tx-1".into(),
                op_count: 3,
            },
        );

        let req = QueryOperationsRequest {
            include_checkpoints: false,
            ..Default::default()
        };
        let resp = svc
            .query_operations(Request::new(req))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.hits.len(), 1, "newer non-checkpoint verb must not be dropped");
        assert_eq!(resp.hits[0].verb, "transaction_commit");
    }

    #[tokio::test]
    async fn query_reads_live_oplog_when_index_is_empty() {
        let (_t, svc) = fresh_service();
        let state = ChangeId::from_bytes([2; 16]);
        write_oplog_record(
            &svc,
            OpRecord::Checkpoint {
                parent: None,
                state,
                thread: Some("main".into()),
            },
        );

        let req = QueryOperationsRequest {
            include_checkpoints: true,
            ..Default::default()
        };
        let resp = svc
            .query_operations(Request::new(req))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(resp.hits.len(), 1);
        assert_eq!(resp.hits[0].verb, "checkpoint");
        assert_eq!(resp.hits[0].thread, "main");
        assert_eq!(resp.hits[0].change_id, state.as_bytes().to_vec());
    }

    #[tokio::test]
    async fn stream_operations_yields_all_hits() {
        let (_t, svc) = fresh_service();
        for i in 0..5u64 {
            write_op(
                &svc,
                make_op(
                    i,
                    1_700_000_000 + (i as i64) * 60,
                    "alice@example.com",
                    "snapshot",
                ),
            );
        }

        let req = StreamOperationsRequest {
            repo_path: String::new(),
            query: Some(QueryOperationsRequest {
                include_checkpoints: true,
                ..Default::default()
            }),
        };
        let resp = svc.stream_operations(Request::new(req)).await.unwrap();

        let mut stream = resp.into_inner();
        let mut collected = Vec::new();
        while let Some(item) = stream.next().await {
            collected.push(item.unwrap());
        }
        assert_eq!(collected.len(), 5);
    }
}
