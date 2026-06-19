// SPDX-License-Identifier: Apache-2.0
//! Local gRPC service for the W2 `OperationLogQueryService`.
//!
//! Read-only wrapper around [`refs::operation_index::OperationLogIndex`].
//! Translates protobuf request/response shapes to/from the index's native
//! [`OperationLogQuery`] / [`IndexedOperation`] types. No state changes,
//! no idempotency wrapper.

use std::pin::Pin;

use chrono::TimeZone;
use futures::Stream;
use grpc::heddle::v1::{
    OperationHit, QueryOperationsRequest, QueryOperationsResponse, StreamOperationsRequest,
    operation_log_query_service_server::OperationLogQueryService,
};
use heddle_query::{IndexedOperation, OperationLogQuery};
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
    heddle_query::apply_checkpoint_filter(&mut q, req.include_checkpoints);
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

#[tonic::async_trait]
impl OperationLogQueryService for LocalOperationLogQueryService {
    type StreamOperationsStream = Pin<Box<dyn Stream<Item = Result<OperationHit, Status>> + Send>>;

    async fn query_operations(
        &self,
        request: Request<QueryOperationsRequest>,
    ) -> Result<Response<QueryOperationsResponse>, Status> {
        let req = request.into_inner();
        let q = build_query(&req);
        let hits = heddle_query::query_operations(self.inner.repo().heddle_dir(), &q)
            .map_err(to_status)?;
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
        let hits = heddle_query::query_operations(self.inner.repo().heddle_dir(), &q)
            .map_err(to_status)?;

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
    use heddle_query::OperationLogIndex;
    use objects::object::ChangeId;
    use oplog::{OpLog, OpRecord};
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
    #[serial_test::serial(process_global)]
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
    #[serial_test::serial(process_global)]
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
    #[serial_test::serial(process_global)]
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

        assert_eq!(
            resp.hits.len(),
            1,
            "newer non-checkpoint verb must not be dropped"
        );
        assert_eq!(resp.hits[0].verb, "transaction_commit");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
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
    #[serial_test::serial(process_global)]
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
