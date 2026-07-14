// SPDX-License-Identifier: Apache-2.0
//! Local-mode gRPC services for `heddle agent serve`.
//!
//! These services implement the gRPC contract over a single local
//! [`Repository`]. They are distinct from `grpc_hosted_impl/` because they
//! - don't require Postgres, Biscuit auth, or the multi-tenant registry,
//! - are reachable over a Unix-domain socket from the same user,
//! - share the dedup/idempotency middleware with the hosted variant via
//!   [`repo::operation_dedup::OperationDedupStore`].
//!
//! Each service has its own file. The shared scaffolding (the
//! [`GrpcLocalService`] struct, idempotency helpers) lives here.

mod signal;
mod state_review;

use std::sync::Arc;

use repo::{
    Repository,
    operation_dedup::{OperationDedupStore, reserve_operation_id_eager},
};
pub use signal::{SignalHealthEntry, SignalHealthReport, get_repo_signal_health};
pub use state_review::LocalStateReviewService;

/// Shared state for the local gRPC services. Handlers borrow the repository
/// for the duration of a single RPC; the dedup store is consulted on every
/// state-changing call.
#[derive(Clone)]
pub struct GrpcLocalService {
    pub(super) repo: Arc<Repository>,
    pub(super) dedup: Arc<OperationDedupStore>,
}

impl GrpcLocalService {
    pub fn new(repo: Arc<Repository>, dedup: Arc<OperationDedupStore>) -> Self {
        Self { repo, dedup }
    }

    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    pub fn dedup(&self) -> &OperationDedupStore {
        &self.dedup
    }
}

/// Idempotency wrapper. Centralises the `check → execute → record` pattern
/// so every state-changing handler folds the same dedup-store flow.
///
/// `client_operation_id` may be empty (caller didn't supply one) — in that
/// case we don't dedup at all and just execute. When supplied, the body
/// must be a deterministic byte representation of the request (typically
/// the protobuf-encoded request).
pub(super) async fn with_idempotency<F, Fut, T>(
    service: &GrpcLocalService,
    client_operation_id: &str,
    verb: &'static str,
    request_body: &[u8],
    execute: F,
) -> Result<T, tonic::Status>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::Status>>,
    T: prost::Message + Default,
{
    use objects::object::OperationId;
    use repo::operation_dedup::{DedupOutcome, hash_request_body};

    if client_operation_id.is_empty() {
        return execute().await;
    }
    let op_id: OperationId = client_operation_id.parse().map_err(|err| {
        tonic::Status::invalid_argument(format!("invalid client_operation_id: {err}"))
    })?;
    let hash = hash_request_body(request_body);
    // The eager reservation atomically claims the (op_id, verb) slot before
    // we run the mutation. Two concurrent retries with the same operation_id
    // can no longer both observe "Fresh" and both apply side effects: the
    // second sees `InFlight` and surfaces a transient `Aborted` to the client.
    let dedup = Arc::clone(&service.dedup);
    let outcome = reserve_operation_id_eager(service.repo(), Arc::clone(&dedup), op_id, verb, hash)
        .map_err(|err| tonic::Status::internal(format!("dedup reserve failed: {err}")))?;
    match outcome {
        DedupOutcome::Replay { response } => T::decode(response.as_slice())
            .map_err(|err| tonic::Status::internal(format!("decode replay failed: {err}"))),
        DedupOutcome::Conflict => Err(tonic::Status::failed_precondition(
            "client_operation_id reused with a different request body",
        )),
        DedupOutcome::InFlight => Err(tonic::Status::aborted(
            "client_operation_id is in flight from another caller; retry once it completes",
        )),
        DedupOutcome::Reserved => {
            // Reservation is held until we either record (success) or
            // cancel (failure). Without the cancel-on-error path, a failed
            // execution would leave a permanent tombstone that all retries
            // would see as `Conflict`/`InFlight` until compaction.
            match execute().await {
                Ok(result) => {
                    let encoded = result.encode_to_vec();
                    dedup.record(op_id, verb, hash, encoded).map_err(|err| {
                        tonic::Status::internal(format!("dedup record failed: {err}"))
                    })?;
                    Ok(result)
                }
                Err(status) => {
                    // Best-effort: if cancel itself fails (disk error etc.)
                    // we still want to surface the original status to the
                    // caller. Compaction will eventually clean a stranded
                    // reservation up.
                    let _ = dedup.cancel(op_id, verb);
                    Err(status)
                }
            }
        }
    }
}

/// Helper for translating a [`HeddleError`](objects::error::HeddleError) into
/// a [`tonic::Status`] with consistent codes across the local services.
pub(super) fn to_status(err: objects::error::HeddleError) -> tonic::Status {
    use objects::error::HeddleError;
    match err {
        HeddleError::NotFound(msg) => tonic::Status::not_found(msg),
        HeddleError::StateNotFound(id) => tonic::Status::not_found(format!("state {id} not found")),
        HeddleError::RepositoryNotFound(path) => {
            tonic::Status::not_found(format!("repository not found at {}", path.display()))
        }
        HeddleError::InvalidObject(msg) => tonic::Status::invalid_argument(msg),
        HeddleError::Conflict(msg) => tonic::Status::failed_precondition(msg),
        HeddleError::Io(io) => tonic::Status::internal(format!("io error: {io}")),
        other => tonic::Status::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end tests for [`with_idempotency`] that exercise the
    //! `Reserved` / `InFlight` / `Replay` / `Conflict` outcomes through the
    //! same wrapper every gRPC handler calls.

    use std::{sync::Arc, time::Duration};

    #[derive(Clone, PartialEq, prost::Message)]
    struct UpdateRefResponse {
        #[prost(string, tag = "1")]
        old_value: String,
    }
    use objects::object::OperationId;
    use repo::{Repository, operation_dedup::OperationDedupStore};
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::{GrpcLocalService, with_idempotency};

    fn make_service() -> (TempDir, GrpcLocalService) {
        let temp = TempDir::new().unwrap();
        let repo = Arc::new(Repository::init_default(temp.path()).unwrap());
        let store = Arc::new(OperationDedupStore::open(repo.heddle_dir()).unwrap());
        (temp, GrpcLocalService::new(repo, store))
    }

    /// A distinguishable prost response payload for the idempotency-flow
    fn marker_response(marker: &str) -> UpdateRefResponse {
        UpdateRefResponse {
            old_value: marker.to_string(),
        }
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn replays_recorded_response() {
        let (_t, service) = make_service();
        let op_id = OperationId::new().to_string();
        let body = b"req";

        // First call executes and records.
        let first = with_idempotency(&service, &op_id, "verb", body, || async {
            Ok::<UpdateRefResponse, tonic::Status>(marker_response("42"))
        })
        .await
        .unwrap();
        assert_eq!(first.old_value, "42");

        // Second call must replay without re-executing — proven by the
        // execute closure panicking if invoked.
        let second = with_idempotency(&service, &op_id, "verb", body, || async {
            #[allow(unreachable_code)]
            Ok::<UpdateRefResponse, tonic::Status>(panic!("execute must not be called on replay"))
        })
        .await
        .unwrap();
        assert_eq!(second.old_value, "42");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn concurrent_calls_with_same_op_id_run_execute_only_once() {
        // The original race window: caller A enters with `Fresh`, awaits
        // execute(), and caller B enters with `Fresh` before A records.
        // Both used to apply side effects. With reservation, B must see
        // `InFlight` and surface `Aborted`.

        let (_t, service) = make_service();
        let op_id = OperationId::new().to_string();
        let body = b"req";

        // We gate the first execution on a oneshot so caller B starts
        // while A is still pending.
        let (tx, rx) = oneshot::channel::<()>();
        let service_a = service.clone();
        let op_a = op_id.clone();
        let a_handle = tokio::spawn(async move {
            with_idempotency(&service_a, &op_a, "verb", body, || async move {
                rx.await.expect("recv gate");
                Ok::<UpdateRefResponse, tonic::Status>(marker_response("7"))
            })
            .await
        });

        // Give A a moment to claim the reservation. The wrapper writes the
        // pending entry synchronously inside the dedup mutex before it
        // awaits, so once we yield the entry is visible.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let service_b = service.clone();
        let op_b = op_id.clone();
        let b_result: Result<UpdateRefResponse, tonic::Status> =
            with_idempotency(&service_b, &op_b, "verb", body, || async {
                panic!("B's execute must not run while A holds the reservation");
            })
            .await;

        // B sees the in-flight reservation and aborts.
        let err = b_result.expect_err("B should be aborted");
        assert_eq!(err.code(), tonic::Code::Aborted);

        // Now release A.
        tx.send(()).unwrap();
        let a_result = a_handle.await.unwrap().unwrap();
        assert_eq!(a_result.old_value, "7");

        // After A finishes, the entry is finalised: a third call with the
        // same body replays.
        let third = with_idempotency(&service, &op_id, "verb", body, || async {
            #[allow(unreachable_code)]
            Ok::<UpdateRefResponse, tonic::Status>(panic!("execute must not run on replay"))
        })
        .await
        .unwrap();
        assert_eq!(third.old_value, "7");
    }

    #[tokio::test]
    #[serial_test::serial(process_global)]
    async fn cancels_reservation_on_execute_failure() {
        // If execute returns Err, the reservation must be released so a
        // retry isn't permanently blocked. Without `cancel`, a transient
        // failure during the first attempt would leave the slot held and
        // every subsequent retry would see Conflict/InFlight until
        // compaction.

        let (_t, service) = make_service();
        let op_id = OperationId::new().to_string();
        let body = b"req";

        let first: Result<UpdateRefResponse, tonic::Status> =
            with_idempotency(&service, &op_id, "verb", body, || async {
                Err(tonic::Status::internal("transient"))
            })
            .await;
        assert!(first.is_err());

        // Retry must succeed — the reservation was released.
        let second = with_idempotency(&service, &op_id, "verb", body, || async {
            Ok::<UpdateRefResponse, tonic::Status>(marker_response("11"))
        })
        .await
        .unwrap();
        assert_eq!(second.old_value, "11");
    }
}
