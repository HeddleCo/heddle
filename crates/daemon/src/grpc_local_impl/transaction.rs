// SPDX-License-Identifier: Apache-2.0
//! Local `TransactionService`.
//!
//! Establishes the *shape* of transactions: a sentinel TOML file under
//! `.heddle/state/transactions/<id>.toml` records that a transaction is
//! active, who started it, what its base state is, and (eventually) which
//! verbs it has buffered. Buffered-op wiring — actually routing CLI verbs
//! through the open transaction so the sentinel collects an ordered list of
//! operations — is follow-on work alongside the rewind-on-abort logic. For
//! now the sentinel is a status object: callers can begin, observe, commit,
//! or abort, but the worktree is not yet rewound on abort and `state_id` on
//! commit is the *base* state, not a freshly produced one.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use grpc::heddle::v1::{
    AbortTransactionRequest, AbortTransactionResponse, BeginTransactionRequest,
    BeginTransactionResponse, CommitTransactionRequest, CommitTransactionResponse,
    GetTransactionStatusRequest, TransactionStatus, transaction_service_server::TransactionService,
};
use objects::{
    fs_atomic::write_file_atomic,
    object::{OperationId, StateId, ThreadName},
};
use oplog::OpRecord;
use prost::Message;
use serde::{Deserialize, Serialize};
use tonic::{Request, Response, Status};

use super::{GrpcLocalService, to_status, with_idempotency};

/// On-disk transaction sentinel.
///
/// Persisted at `<heddle_dir>/state/transactions/<transaction_id>.toml`. The
/// sentinel's lifecycle mirrors the RPC surface: written on `begin`, mutated
/// in place by `commit`/`abort`, and read by `get_status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionSentinel {
    transaction_id: String,
    repo_path: String,
    thread: String,
    message: String,
    /// "active" | "committed" | "aborted".
    state: String,
    started_at_secs: i64,
    started_by_email: String,
    /// Full `StateId` at begin time, recorded so a future rewind has a target.
    base_state: String,
    /// Verb names buffered into the transaction. Empty for now — CLI verbs
    /// do not yet route through the open transaction; that wiring is
    /// follow-on work.
    buffered_ops: Vec<String>,
    /// Reason supplied via `AbortTransactionRequest::reason`.
    aborted_reason: Option<String>,
}

const STATE_ACTIVE: &str = "active";
const STATE_COMMITTED: &str = "committed";
const STATE_ABORTED: &str = "aborted";

fn parse_transaction_id(raw: &str) -> Result<OperationId, Status> {
    let transaction_id: OperationId = raw
        .parse()
        .map_err(|err| Status::invalid_argument(format!("invalid transaction_id: {err}")))?;
    if transaction_id.to_string() != raw {
        return Err(Status::invalid_argument(
            "invalid transaction_id: expected canonical UUID",
        ));
    }
    Ok(transaction_id)
}

/// Build the on-disk sentinel path for a transaction id.
///
/// Taking [`OperationId`] keeps unvalidated wire strings from reaching
/// filesystem path construction.
fn sentinel_path(repo: &repo::Repository, transaction_id: &OperationId) -> PathBuf {
    repo.heddle_dir()
        .join("state")
        .join("transactions")
        .join(format!("{transaction_id}.toml"))
}

/// Read and parse the sentinel for `path`, mapping I/O and parse errors to
/// `tonic::Status`.
fn load_sentinel(path: &Path) -> Result<TransactionSentinel, Status> {
    let bytes = std::fs::read(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            Status::not_found(format!(
                "transaction sentinel not found at {}",
                path.display()
            ))
        } else {
            Status::internal(format!("read sentinel failed: {err}"))
        }
    })?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|err| Status::internal(format!("sentinel not utf8: {err}")))?;
    toml::from_str(text).map_err(|err| Status::internal(format!("sentinel parse failed: {err}")))
}

/// Atomically write the sentinel to `path`.
fn save_sentinel(path: &Path, sentinel: &TransactionSentinel) -> Result<(), Status> {
    let serialized = toml::to_string_pretty(sentinel)
        .map_err(|err| Status::internal(format!("sentinel serialize failed: {err}")))?;
    write_file_atomic(path, serialized.as_bytes())
        .map_err(|err| Status::internal(format!("sentinel write failed: {err}")))
}

/// Wall-clock seconds since the UNIX epoch.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local-mode `TransactionService` impl.
///
/// Wraps the shared [`GrpcLocalService`] state so the dedup store and
/// repository handle are available to every RPC.
#[derive(Clone)]
pub struct LocalTransactionService {
    inner: GrpcLocalService,
}

impl LocalTransactionService {
    pub fn new(inner: GrpcLocalService) -> Self {
        Self { inner }
    }
}

#[tonic::async_trait]
impl TransactionService for LocalTransactionService {
    async fn begin_transaction(
        &self,
        request: Request<BeginTransactionRequest>,
    ) -> Result<Response<BeginTransactionResponse>, Status> {
        let req = request.into_inner();
        let request_body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TransactionService.BeginTransaction",
            &request_body,
            move || async move {
                let repo = inner.repo();

                // Resolve base_state from the request's thread (if non-empty)
                // or from current HEAD. Either path can produce `None` if the
                // repository has no snapshots yet — tests therefore seed at
                // least one snapshot before calling `begin_transaction`.
                let base_state_id = if !req.thread.is_empty() {
                    repo.refs()
                        .get_thread(&ThreadName::from(req.thread.as_str()))
                        .map_err(to_status)?
                } else {
                    repo.head().map_err(to_status)?
                };
                let base_state = base_state_id
                    .ok_or_else(|| {
                        Status::failed_precondition(
                            "cannot begin transaction: no base state (repository has no snapshots)",
                        )
                    })?
                    .to_string_full();

                let started_by_email = repo.get_principal().map(|p| p.email).unwrap_or_default();
                let started_at_secs = now_secs();
                let transaction_id = OperationId::new();
                let transaction_id_wire = transaction_id.to_string();

                let sentinel = TransactionSentinel {
                    transaction_id: transaction_id_wire.clone(),
                    repo_path: req.repo_path.clone(),
                    thread: req.thread.clone(),
                    message: req.message.clone(),
                    state: STATE_ACTIVE.to_string(),
                    started_at_secs,
                    started_by_email,
                    base_state,
                    buffered_ops: Vec::new(),
                    aborted_reason: None,
                };
                let path = sentinel_path(repo, &transaction_id);
                save_sentinel(&path, &sentinel)?;

                Ok(BeginTransactionResponse {
                    transaction_id: transaction_id_wire,
                    started_at: Some(prost_types::Timestamp {
                        seconds: started_at_secs,
                        nanos: 0,
                    }),
                })
            },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn commit_transaction(
        &self,
        request: Request<CommitTransactionRequest>,
    ) -> Result<Response<CommitTransactionResponse>, Status> {
        let req = request.into_inner();
        let transaction_id = parse_transaction_id(&req.transaction_id)?;
        let request_body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TransactionService.CommitTransaction",
            &request_body,
            move || async move {
                let repo = inner.repo();
                let path = sentinel_path(repo, &transaction_id);
                let mut sentinel = load_sentinel(&path)?;

                if sentinel.state != STATE_ACTIVE {
                    return Err(Status::failed_precondition(format!(
                        "transaction already {}",
                        sentinel.state
                    )));
                }

                // Capture the buffered op count, drain the list so a
                // re-run cannot double-replay, flip the sentinel, and
                // append `OpRecord::TransactionCommit` to the oplog. Real
                // per-op replay (executing the buffered verbs at commit
                // time rather than at call time) is the next follow-on.
                //
                // heddle#382 boundary: the daemon transaction service is not a
                // local AtomicMutation root and remains outside the same-thread
                // CAS-on-commit guarantee until this service is migrated to the
                // conditional oplog API or an AtomicMutation-backed flow.
                let op_count = sentinel.buffered_ops.len() as u32;
                let transaction_id = sentinel.transaction_id.clone();
                sentinel.state = STATE_COMMITTED.to_string();
                sentinel.buffered_ops.clear();
                save_sentinel(&path, &sentinel)?;

                if let Err(err) = repo.oplog().record_batch(vec![OpRecord::TransactionCommit {
                    transaction_id,
                    op_count,
                }]) {
                    tracing::warn!(error = %err, txn = %sentinel.transaction_id,
                        "transaction-service: failed to record TransactionCommit");
                }

                Ok(CommitTransactionResponse {
                    // `base_state` is a hex-display string in the sentinel
                    // file; decode back to bytes for the wire response.
                    state_id: StateId::parse(&sentinel.base_state)
                        .map(|id| id.as_bytes().to_vec())
                        .unwrap_or_default(),
                    op_count,
                })
            },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn abort_transaction(
        &self,
        request: Request<AbortTransactionRequest>,
    ) -> Result<Response<AbortTransactionResponse>, Status> {
        let req = request.into_inner();
        let transaction_id = parse_transaction_id(&req.transaction_id)?;
        let request_body = req.encode_to_vec();
        let client_op = req.client_operation_id.clone();
        let inner = self.inner.clone();

        let response = with_idempotency(
            &self.inner,
            &client_op,
            "TransactionService.AbortTransaction",
            &request_body,
            move || async move {
                let repo = inner.repo();
                let path = sentinel_path(repo, &transaction_id);
                let mut sentinel = load_sentinel(&path)?;

                if sentinel.state != STATE_ACTIVE {
                    return Err(Status::failed_precondition(format!(
                        "transaction already {}",
                        sentinel.state
                    )));
                }

                let reason = if req.reason.is_empty() {
                    None
                } else {
                    Some(req.reason.clone())
                };
                let transaction_id = sentinel.transaction_id.clone();
                sentinel.state = STATE_ABORTED.to_string();
                sentinel.aborted_reason = reason.clone();
                // Drain buffered ops on abort too — the abort is now
                // the terminal state, and future reads of this sentinel
                // shouldn't surface the list as still-pending work.
                sentinel.buffered_ops.clear();
                save_sentinel(&path, &sentinel)?;

                // Record `OpRecord::TransactionAbort` so the abort shows
                // up in the audit trail. Worktree rewind to `base_state`
                // is follow-on work — today the worktree stays where the
                // (still-execute-immediately) buffered verbs left it.
                if let Err(err) = repo.oplog().record_batch(vec![OpRecord::TransactionAbort {
                    transaction_id,
                    reason: reason.unwrap_or_default(),
                }]) {
                    tracing::warn!(error = %err, txn = %sentinel.transaction_id,
                        "transaction-service: failed to record TransactionAbort");
                }

                Ok(AbortTransactionResponse { aborted: true })
            },
        )
        .await?;

        Ok(Response::new(response))
    }

    async fn get_transaction_status(
        &self,
        request: Request<GetTransactionStatusRequest>,
    ) -> Result<Response<TransactionStatus>, Status> {
        let req = request.into_inner();
        let transaction_id = parse_transaction_id(&req.transaction_id)?;
        let repo = self.inner.repo();
        let path = sentinel_path(repo, &transaction_id);
        let sentinel = load_sentinel(&path)?;

        Ok(Response::new(TransactionStatus {
            transaction_id: sentinel.transaction_id,
            state: sentinel.state,
            started_at: Some(prost_types::Timestamp {
                seconds: sentinel.started_at_secs,
                nanos: 0,
            }),
            buffered_ops: sentinel.buffered_ops.len() as u32,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use oplog::OpLogBackend;
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;

    use super::*;

    /// Build a repository with at least one snapshot (so HEAD is non-empty)
    /// and wrap it in a [`LocalTransactionService`] for direct RPC calls.
    fn build_service() -> (TempDir, LocalTransactionService) {
        let tmp = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(tmp.path()).expect("init repo");
        // `init_default` already seeds the empty-tree snapshot on `main`, so
        // HEAD resolves to a real StateId.
        assert!(repo.head().expect("head").is_some(), "head should be set");
        let dedup = OperationDedupStore::open(repo.heddle_dir()).expect("dedup open");
        let service = GrpcLocalService::new(Arc::new(repo), Arc::new(dedup));
        (tmp, LocalTransactionService::new(service))
    }

    fn begin_req() -> BeginTransactionRequest {
        BeginTransactionRequest {
            repo_path: String::new(),
            thread: String::new(),
            message: "test txn".to_string(),
            client_operation_id: String::new(),
        }
    }

    fn parse_begin_id(raw: &str) -> OperationId {
        raw.parse()
            .expect("begin_transaction should return an OperationId")
    }

    fn legacy_string_sentinel_path(repo: &Repository, transaction_id: &str) -> PathBuf {
        repo.heddle_dir()
            .join("state")
            .join("transactions")
            .join(format!("{transaction_id}.toml"))
    }

    fn write_trap_sentinel(path: &Path) -> Vec<u8> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("trap parent");
        }
        let sentinel = TransactionSentinel {
            transaction_id: OperationId::new().to_string(),
            repo_path: "trap".to_string(),
            thread: String::new(),
            message: "must not be touched".to_string(),
            state: STATE_ACTIVE.to_string(),
            started_at_secs: 1,
            started_by_email: "trap@example.com".to_string(),
            base_state: StateId::from_bytes([0; 32]).to_string_full(),
            buffered_ops: vec!["trap-op".to_string()],
            aborted_reason: None,
        };
        let body = toml::to_string_pretty(&sentinel).expect("serialize trap");
        fs::write(path, body.as_bytes()).expect("write trap");
        body.into_bytes()
    }

    async fn assert_invalid_transaction_id_rejected(svc: &LocalTransactionService, raw: &str) {
        let commit_err = svc
            .commit_transaction(Request::new(CommitTransactionRequest {
                repo_path: String::new(),
                transaction_id: raw.to_string(),
                client_operation_id: String::new(),
            }))
            .await
            .expect_err("commit must reject invalid transaction_id");
        assert_eq!(commit_err.code(), tonic::Code::InvalidArgument);
        assert!(
            commit_err.message().contains("invalid transaction_id"),
            "unexpected commit error: {commit_err}"
        );

        let abort_err = svc
            .abort_transaction(Request::new(AbortTransactionRequest {
                repo_path: String::new(),
                transaction_id: raw.to_string(),
                reason: "nope".to_string(),
                client_operation_id: String::new(),
            }))
            .await
            .expect_err("abort must reject invalid transaction_id");
        assert_eq!(abort_err.code(), tonic::Code::InvalidArgument);
        assert!(
            abort_err.message().contains("invalid transaction_id"),
            "unexpected abort error: {abort_err}"
        );

        let status_err = svc
            .get_transaction_status(Request::new(GetTransactionStatusRequest {
                repo_path: String::new(),
                transaction_id: raw.to_string(),
            }))
            .await
            .expect_err("status must reject invalid transaction_id");
        assert_eq!(status_err.code(), tonic::Code::InvalidArgument);
        assert!(
            status_err.message().contains("invalid transaction_id"),
            "unexpected status error: {status_err}"
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn begin_creates_active_sentinel() {
        let (_tmp, svc) = build_service();
        let resp = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();
        assert!(!resp.transaction_id.is_empty());
        assert!(resp.started_at.as_ref().map(|t| t.seconds).unwrap_or(0) > 0);

        let status = svc
            .get_transaction_status(Request::new(GetTransactionStatusRequest {
                repo_path: String::new(),
                transaction_id: resp.transaction_id.clone(),
            }))
            .await
            .expect("status")
            .into_inner();
        assert_eq!(status.state, STATE_ACTIVE);
        assert_eq!(status.buffered_ops, 0);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn commit_flips_state_to_committed() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();

        let commit = svc
            .commit_transaction(Request::new(CommitTransactionRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id.clone(),
                client_operation_id: String::new(),
            }))
            .await
            .expect("commit")
            .into_inner();
        assert!(!commit.state_id.is_empty());
        assert_eq!(commit.op_count, 0);

        let status = svc
            .get_transaction_status(Request::new(GetTransactionStatusRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id,
            }))
            .await
            .expect("status")
            .into_inner();
        assert_eq!(status.state, STATE_COMMITTED);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn abort_records_reason() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();

        let abort = svc
            .abort_transaction(Request::new(AbortTransactionRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id.clone(),
                reason: "user cancelled".to_string(),
                client_operation_id: String::new(),
            }))
            .await
            .expect("abort")
            .into_inner();
        assert!(abort.aborted);

        // Read the sentinel back via the loader to confirm `aborted_reason`
        // round-trips through TOML.
        let transaction_id = parse_begin_id(&begin.transaction_id);
        let path = sentinel_path(svc.inner.repo(), &transaction_id);
        let sentinel = load_sentinel(&path).expect("load");
        assert_eq!(sentinel.state, STATE_ABORTED);
        assert_eq!(sentinel.aborted_reason.as_deref(), Some("user cancelled"));
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn commit_after_abort_returns_failed_precondition() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();
        svc.abort_transaction(Request::new(AbortTransactionRequest {
            repo_path: String::new(),
            transaction_id: begin.transaction_id.clone(),
            reason: String::new(),
            client_operation_id: String::new(),
        }))
        .await
        .expect("abort");

        let err = svc
            .commit_transaction(Request::new(CommitTransactionRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id,
                client_operation_id: String::new(),
            }))
            .await
            .expect_err("commit must fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn get_status_returns_current_state() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();

        let status = svc
            .get_transaction_status(Request::new(GetTransactionStatusRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id.clone(),
            }))
            .await
            .expect("status")
            .into_inner();
        assert_eq!(status.transaction_id, begin.transaction_id);
        assert_eq!(status.state, STATE_ACTIVE);
        assert_eq!(status.started_at, begin.started_at);
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn commit_clears_buffered_ops_and_records_oplog_entry() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();

        // Hand-write a couple of buffered ops onto the sentinel —
        // mirrors what the CLI front-end does today
        // (`append_op_to_active_for_thread`).
        let transaction_id = parse_begin_id(&begin.transaction_id);
        let path = sentinel_path(svc.inner.repo(), &transaction_id);
        let mut sentinel = load_sentinel(&path).expect("load");
        sentinel.buffered_ops = vec!["capture".into(), "merge".into()];
        save_sentinel(&path, &sentinel).expect("save");

        // Snapshot the oplog tail length so we can pick out the entry
        // commit_transaction appends.
        let before_tail_len = svc
            .inner
            .repo()
            .oplog()
            .recent(64)
            .expect("oplog recent")
            .len();

        let commit = svc
            .commit_transaction(Request::new(CommitTransactionRequest {
                repo_path: String::new(),
                transaction_id: begin.transaction_id.clone(),
                client_operation_id: String::new(),
            }))
            .await
            .expect("commit")
            .into_inner();
        assert_eq!(commit.op_count, 2, "wire response carries the count");

        // Sentinel: buffered list drained, state flipped.
        let after = load_sentinel(&path).expect("load after commit");
        assert_eq!(after.state, STATE_COMMITTED);
        assert!(
            after.buffered_ops.is_empty(),
            "commit must drain buffered_ops so a re-run cannot double-replay"
        );

        // Oplog: a TransactionCommit entry pinned to this transaction id
        // with the captured count is present in the tail.
        let tail = svc.inner.repo().oplog().recent(64).expect("oplog recent");
        assert!(
            tail.len() > before_tail_len,
            "commit should have appended at least one oplog entry"
        );
        let last = tail.last().expect("non-empty tail");
        match &last.operation {
            oplog::OpRecord::TransactionCommit {
                transaction_id,
                op_count,
            } => {
                assert_eq!(transaction_id, &begin.transaction_id);
                assert_eq!(*op_count, 2);
            }
            other => panic!("expected TransactionCommit at oplog tail, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn abort_records_oplog_entry_with_reason() {
        let (_tmp, svc) = build_service();
        let begin = svc
            .begin_transaction(Request::new(begin_req()))
            .await
            .expect("begin")
            .into_inner();

        let before_tail_len = svc
            .inner
            .repo()
            .oplog()
            .recent(64)
            .expect("oplog recent")
            .len();

        svc.abort_transaction(Request::new(AbortTransactionRequest {
            repo_path: String::new(),
            transaction_id: begin.transaction_id.clone(),
            reason: "rollback please".to_string(),
            client_operation_id: String::new(),
        }))
        .await
        .expect("abort");

        let tail = svc.inner.repo().oplog().recent(64).expect("oplog recent");
        assert!(
            tail.len() > before_tail_len,
            "abort should have appended at least one oplog entry"
        );
        let last = tail.last().expect("non-empty tail");
        match &last.operation {
            oplog::OpRecord::TransactionAbort {
                transaction_id,
                reason,
            } => {
                assert_eq!(transaction_id, &begin.transaction_id);
                assert_eq!(reason, "rollback please");
            }
            other => panic!("expected TransactionAbort at oplog tail, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn begin_idempotent_returns_same_transaction_id() {
        let (_tmp, svc) = build_service();
        let client_op = OperationId::new().to_string();

        let mut req = begin_req();
        req.client_operation_id = client_op.clone();

        let first = svc
            .begin_transaction(Request::new(req.clone()))
            .await
            .expect("begin1")
            .into_inner();
        let second = svc
            .begin_transaction(Request::new(req))
            .await
            .expect("begin2")
            .into_inner();
        assert_eq!(first.transaction_id, second.transaction_id);
        assert_eq!(first.started_at, second.started_at);
    }

    #[test]
    #[serial_test::serial(process_global)]
    fn sentinel_path_is_derived_from_operation_id() {
        let (_tmp, svc) = build_service();
        let transaction_id = OperationId::new();
        let path = sentinel_path(svc.inner.repo(), &transaction_id);
        let expected_file_name = format!("{transaction_id}.toml");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(expected_file_name.as_str())
        );
        assert!(
            path.starts_with(
                svc.inner
                    .repo()
                    .heddle_dir()
                    .join("state")
                    .join("transactions")
            )
        );
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn invalid_transaction_ids_are_rejected_before_sentinel_path_io() {
        let (tmp, svc) = build_service();
        let absolute = tmp.path().join("outside-absolute").display().to_string();
        let invalid_ids = [
            "../../x".to_string(),
            "a/b".to_string(),
            "..".to_string(),
            absolute,
            String::new(),
        ];

        for raw in invalid_ids {
            let trap_path = legacy_string_sentinel_path(svc.inner.repo(), &raw);
            let before = write_trap_sentinel(&trap_path);

            assert_invalid_transaction_id_rejected(&svc, &raw).await;

            let after = fs::read(&trap_path).expect("trap should still exist");
            assert_eq!(
                after,
                before,
                "invalid transaction_id {raw:?} must not touch {}",
                trap_path.display()
            );
        }
    }
}
