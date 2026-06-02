// SPDX-License-Identifier: Apache-2.0
//! Postgres-backed operation log for the stateless server.
//!
//! Each operation is a row in the `oplog` table. Batches share a `batch_id`.
//! Undo/redo state is tracked with the `undone` column.

#![cfg(feature = "postgres")]

use std::{
    io,
    sync::{Arc, OnceLock},
};

use chrono::{DateTime, Utc};
use objects::error::{HeddleError, Result};
use runtime_bridge::RuntimeBridge;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    oplog_backend::OpLogBackend,
    oplog_types::{
        ConditionalCommitOutcome, IsolationPrecondition, OpBatch, OpEntry, OpRecord,
        isolation_keys_for_record,
    },
};

fn sqlx_err(e: sqlx::Error) -> HeddleError {
    HeddleError::Io(std::io::Error::other(e.to_string()))
}

/// Postgres-backed operation log backend for the stateless server.
///
/// The `async` batch reads (`recent_batches_scoped`, `undo_batches_scoped`,
/// `redo_batches_scoped`) `.await` `sqlx` directly — no runtime bridge. The
/// remaining synchronous `OpLogBackend` methods still drive their `sqlx`
/// futures through a shared [`RuntimeBridge`] so they're safe to call from
/// any caller flavor — including a current-thread Tokio runtime and
/// non-Tokio threads. See [`PgOpLogBackend::bridge`] for the lazy-init
/// pattern.
#[derive(Clone)]
pub struct PgOpLogBackend {
    pool: Arc<PgPool>,
    repo_id: Uuid,
    /// Lazy worker-thread + private Tokio runtime that drives the sync
    /// `OpLogBackend` surface. Wrapped in `Arc<OnceLock<_>>` so every
    /// clone of `PgOpLogBackend` shares one bridge (one worker thread),
    /// and the spawn cost is paid on first sync use.
    bridge: Arc<OnceLock<RuntimeBridge>>,
}

impl PgOpLogBackend {
    pub fn new(pool: Arc<PgPool>, repo_id: Uuid) -> Self {
        Self {
            pool,
            repo_id,
            bridge: Arc::new(OnceLock::new()),
        }
    }

    /// Lazily-initialized accessor for the runtime bridge.
    ///
    /// The pre-fix `block_in_place(Handle::current().block_on(...))` path
    /// was only valid on a multi-thread Tokio runtime; on a
    /// `current_thread` runtime (e.g. `#[tokio::test(flavor =
    /// "current_thread")]`) it panicked with `"can call blocking only
    /// when running on the multi-threaded runtime"`, and on a non-Tokio
    /// thread the inner `Handle::current()` panicked outright. Routing
    /// through the bridge sidesteps both: the bridge's own current-thread
    /// runtime polls the future regardless of who called.
    fn bridge(&self) -> Result<&RuntimeBridge> {
        if let Some(bridge) = self.bridge.get() {
            return Ok(bridge);
        }
        let new = RuntimeBridge::with_thread_name("heddle-pg-oplog-bridge").map_err(|err| {
            HeddleError::Io(io::Error::other(format!(
                "pg-oplog runtime bridge: spawn worker thread: {err}",
            )))
        })?;
        // If a concurrent caller already populated the slot, `set` drops
        // our worker; its tx side dies with it and the spawned thread
        // exits cleanly when `rx.recv()` returns Err. First-use only, so
        // the wasted spawn is acceptable in exchange for keeping
        // `bridge()` lock-free on the hot path.
        let _ = self.bridge.set(new);
        Ok(self
            .bridge
            .get()
            .expect("OnceLock populated above or by a concurrent caller"))
    }

    fn block<F, T>(&self, f: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        // `block_on` returns `Result<Result<T, HeddleError>, BridgeError>`:
        // the outer error reports worker liveness (dead / lost reply), the
        // inner error is the sqlx Result the future produces. Both are
        // surfaced to callers as `HeddleError::Io`; the bridge error
        // string identifies which liveness mode tripped so a stuck
        // PgPool isn't confused with a panicked worker.
        self.bridge()?.block_on(f).map_err(|err| {
            HeddleError::Io(io::Error::other(format!("pg-oplog runtime bridge: {err}")))
        })?
    }

    fn row_to_entry(r: &sqlx::postgres::PgRow) -> Result<OpEntry> {
        let id: i64 = r.try_get("id").map_err(sqlx_err)?;
        let batch_id: i64 = r.try_get("batch_id").map_err(sqlx_err)?;
        let batch_index: i32 = r.try_get("batch_index").map_err(sqlx_err)?;
        let op_data: serde_json::Value = r.try_get("op_data").map_err(sqlx_err)?;
        let undone: bool = r.try_get("undone").map_err(sqlx_err)?;
        let created_at: DateTime<Utc> = r.try_get("created_at").map_err(sqlx_err)?;

        let operation: OpRecord = serde_json::from_value(op_data)
            .map_err(|e| HeddleError::Serialization(e.to_string()))?;
        let scope: Option<String> = r.try_get("scope").map_err(sqlx_err)?;

        // The Postgres `oplog` table doesn't yet carry actor/operation_id
        // columns; that schema migration lands with the per-call principal
        // threading work. Until then, hosted reads surface a placeholder
        // attribution. This is the only remaining placeholder after the
        // cleanup pass — file-backed oplog already plumbs the real actor.
        Ok(OpEntry {
            id: id as u64,
            timestamp: created_at,
            operation,
            undone,
            batch_id: batch_id as u64,
            batch_index: batch_index as u32,
            scope,
            actor: std::sync::Arc::new(objects::object::Principal::new("<unknown>", "")),
            operation_id: None,
        })
    }

    fn group_into_batches(entries: Vec<OpEntry>) -> Vec<OpBatch> {
        let mut batches: Vec<OpBatch> = Vec::new();
        for entry in entries {
            if let Some(batch) = batches.last_mut()
                && batch.id == entry.batch_id
            {
                batch.entries.push(entry);
            } else {
                batches.push(OpBatch {
                    id: entry.batch_id,
                    entries: vec![entry],
                });
            }
        }
        batches
    }

    async fn fetch_batches_by_ids(
        pool: &PgPool,
        repo_id: Uuid,
        batch_ids: &[i64],
        asc: bool,
    ) -> Result<Vec<OpBatch>> {
        if batch_ids.is_empty() {
            return Ok(Vec::new());
        }
        let order = if asc { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT id, batch_id, batch_index, op_data, undone, created_at, scope
             FROM oplog WHERE repo_id = $1 AND batch_id = ANY($2)
             ORDER BY batch_id {order}, batch_index ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(repo_id)
            .bind(batch_ids)
            .fetch_all(pool)
            .await
            .map_err(sqlx_err)?;
        let entries: Result<Vec<OpEntry>> = rows.iter().map(Self::row_to_entry).collect();
        Ok(Self::group_into_batches(entries?))
    }

    async fn allocate_batch_id(pool: &PgPool, repo_id: Uuid) -> Result<i64> {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO oplog_batch_counters (repo_id, next_batch_id, updated_at)
             VALUES ($1, 2, NOW())
             ON CONFLICT (repo_id)
             DO UPDATE SET
               next_batch_id = oplog_batch_counters.next_batch_id + 1,
               updated_at = NOW()
             RETURNING next_batch_id - 1",
        )
        .bind(repo_id)
        .fetch_one(pool)
        .await
        .map_err(sqlx_err)
    }

    /// Native-async batch fetch shared by `recent`/`undo`/`redo`. Borrows
    /// `self`, `extra_where`, and `order` for the duration of the returned
    /// future — no `'static` worker-thread bridge — so the hosted server
    /// awaits `sqlx` directly.
    async fn fetch_scoped_batches(
        &self,
        count: usize,
        scope: Option<&str>,
        extra_where: &str,
        order: &str,
        asc: bool,
    ) -> Result<Vec<OpBatch>> {
        let pool = self.pool.as_ref();
        let repo_id = self.repo_id;
        let batch_ids: Vec<i64> = if let Some(scope) = scope {
            let sql = format!(
                "SELECT DISTINCT batch_id FROM oplog
                 WHERE repo_id = $1 {extra_where} AND scope = $2
                 ORDER BY batch_id {order} LIMIT $3"
            );
            sqlx::query_scalar::<_, i64>(&sql)
                .bind(repo_id)
                .bind(scope)
                .bind(count as i64)
                .fetch_all(pool)
                .await
                .map_err(sqlx_err)?
        } else {
            let sql = format!(
                "SELECT DISTINCT batch_id FROM oplog
                 WHERE repo_id = $1 {extra_where}
                 ORDER BY batch_id {order} LIMIT $2"
            );
            sqlx::query_scalar::<_, i64>(&sql)
                .bind(repo_id)
                .bind(count as i64)
                .fetch_all(pool)
                .await
                .map_err(sqlx_err)?
        };
        Self::fetch_batches_by_ids(pool, repo_id, &batch_ids, asc).await
    }
}

impl OpLogBackend for PgOpLogBackend {
    fn record_batch_scoped(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
    ) -> Result<Vec<u64>> {
        if operations.is_empty() {
            return Ok(Vec::new());
        }
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let scope = scope.map(str::to_string);
        self.block(async move {
            let mut tx = pool.begin().await.map_err(sqlx_err)?;

            let batch_id = Self::allocate_batch_id(&pool, repo_id).await?;

            let mut ids = Vec::with_capacity(operations.len());
            for (index, op) in operations.iter().enumerate() {
                let op_data = serde_json::to_value(op)
                    .map_err(|e| HeddleError::Serialization(e.to_string()))?;
                let id: i64 = sqlx::query_scalar::<_, i64>(
                    "INSERT INTO oplog (repo_id, batch_id, batch_index, op_data, undone, scope)
                     VALUES ($1, $2, $3, $4, false, $5)
                     RETURNING id",
                )
                .bind(repo_id)
                .bind(batch_id)
                .bind(index as i32)
                .bind(op_data)
                .bind(scope.as_deref())
                .fetch_one(&mut *tx)
                .await
                .map_err(sqlx_err)?;
                ids.push(id as u64);
            }

            tx.commit().await.map_err(sqlx_err)?;
            Ok(ids)
        })
    }

    fn record_batch_exactly_once_if_unchanged(
        &self,
        operations: Vec<OpRecord>,
        scope: Option<&str>,
        transaction_id: &str,
        precondition: &IsolationPrecondition,
    ) -> Result<ConditionalCommitOutcome> {
        if operations.is_empty() {
            return Ok(ConditionalCommitOutcome::Committed(Vec::new()));
        }

        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let scope = scope.map(str::to_string);
        let transaction_id = transaction_id.to_string();
        let precondition = precondition.clone();

        self.block(async move {
            let mut tx = pool.begin().await.map_err(sqlx_err)?;

            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(repo_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(sqlx_err)?;

            let committed_batch_id: Option<i64> = sqlx::query_scalar(
                "SELECT batch_id FROM oplog
                 WHERE repo_id = $1
                   AND op_data->'TransactionCommit'->>'transaction_id' = $2
                 ORDER BY id ASC
                 LIMIT 1",
            )
            .bind(repo_id)
            .bind(&transaction_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_err)?;

            if let Some(batch_id) = committed_batch_id {
                let rows = sqlx::query(
                    "SELECT id, batch_id, batch_index, op_data, undone, created_at, scope
                     FROM oplog
                     WHERE repo_id = $1 AND batch_id = $2
                     ORDER BY batch_index ASC",
                )
                .bind(repo_id)
                .bind(batch_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_err)?;
                let records = rows
                    .iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|entry| !matches!(entry.operation, OpRecord::TransactionCommit { .. }))
                    .map(|entry| entry.operation)
                    .collect();
                tx.rollback().await.map_err(sqlx_err)?;
                return Ok(ConditionalCommitOutcome::AlreadyCommitted(records));
            }

            let current_head_id: i64 =
                sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM oplog WHERE repo_id = $1")
                    .bind(repo_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(sqlx_err)?;

            if !precondition.keys.is_empty() && current_head_id as u64 != precondition.since_head_id
            {
                let rows = sqlx::query(
                    "SELECT id, batch_id, batch_index, op_data, undone, created_at, scope
                     FROM oplog
                     WHERE repo_id = $1 AND id > $2
                     ORDER BY id ASC",
                )
                .bind(repo_id)
                .bind(precondition.since_head_id as i64)
                .fetch_all(&mut *tx)
                .await
                .map_err(sqlx_err)?;
                for entry in rows
                    .iter()
                    .map(Self::row_to_entry)
                    .collect::<Result<Vec<_>>>()?
                {
                    let touched =
                        isolation_keys_for_record(&entry.operation, entry.scope.as_deref());
                    if let Some(key) = touched.intersection(&precondition.keys).next().cloned() {
                        tx.rollback().await.map_err(sqlx_err)?;
                        return Ok(ConditionalCommitOutcome::IsolationConflict {
                            key,
                            since_head_id: precondition.since_head_id,
                            conflicting_entry_id: entry.id,
                        });
                    }
                }
            }

            let batch_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO oplog_batch_counters (repo_id, next_batch_id, updated_at)
                 VALUES ($1, 2, NOW())
                 ON CONFLICT (repo_id)
                 DO UPDATE SET
                   next_batch_id = oplog_batch_counters.next_batch_id + 1,
                   updated_at = NOW()
                 RETURNING next_batch_id - 1",
            )
            .bind(repo_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(sqlx_err)?;

            let mut ids = Vec::with_capacity(operations.len());
            for (index, op) in operations.iter().enumerate() {
                let op_data = serde_json::to_value(op)
                    .map_err(|e| HeddleError::Serialization(e.to_string()))?;
                let id: i64 = sqlx::query_scalar::<_, i64>(
                    "INSERT INTO oplog (repo_id, batch_id, batch_index, op_data, undone, scope)
                     VALUES ($1, $2, $3, $4, false, $5)
                     RETURNING id",
                )
                .bind(repo_id)
                .bind(batch_id)
                .bind(index as i32)
                .bind(op_data)
                .bind(scope.as_deref())
                .fetch_one(&mut *tx)
                .await
                .map_err(sqlx_err)?;
                ids.push(id as u64);
            }

            tx.commit().await.map_err(sqlx_err)?;
            Ok(ConditionalCommitOutcome::Committed(ids))
        })
    }

    fn last(&self) -> Result<Option<OpEntry>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        self.block(async move {
            let row = sqlx::query(
                "SELECT id, batch_id, batch_index, op_data, undone, created_at, scope
                 FROM oplog WHERE repo_id = $1 ORDER BY id DESC LIMIT 1",
            )
            .bind(repo_id)
            .fetch_optional(pool.as_ref())
            .await
            .map_err(sqlx_err)?;
            row.map(|r| Self::row_to_entry(&r)).transpose()
        })
    }

    fn recent(&self, count: usize) -> Result<Vec<OpEntry>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        self.block(async move {
            let rows = sqlx::query(
                "SELECT id, batch_id, batch_index, op_data, undone, created_at, scope
                 FROM oplog WHERE repo_id = $1 ORDER BY id DESC LIMIT $2",
            )
            .bind(repo_id)
            .bind(count as i64)
            .fetch_all(pool.as_ref())
            .await
            .map_err(sqlx_err)?;
            rows.iter().map(Self::row_to_entry).collect()
        })
    }

    async fn recent_batches_scoped(
        &self,
        count: usize,
        scope: Option<&str>,
    ) -> Result<Vec<OpBatch>> {
        self.fetch_scoped_batches(count, scope, "", "DESC", false)
            .await
    }

    async fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.fetch_scoped_batches(count, scope, "AND undone = false", "DESC", false)
            .await
    }

    async fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        self.fetch_scoped_batches(count, scope, "AND undone = true", "ASC", true)
            .await
    }

    fn mark_batch_undone(&self, batch: &OpBatch) -> Result<OpBatch> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let batch_id = batch.id as i64;
        self.block(async move {
            sqlx::query("UPDATE oplog SET undone = true WHERE repo_id = $1 AND batch_id = $2")
                .bind(repo_id)
                .bind(batch_id)
                .execute(pool.as_ref())
                .await
                .map_err(sqlx_err)?;
            Ok(())
        })?;
        let mut updated = batch.clone();
        for entry in &mut updated.entries {
            entry.undone = true;
        }
        Ok(updated)
    }

    fn mark_batch_redone(&self, batch: &OpBatch) -> Result<OpBatch> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let batch_id = batch.id as i64;
        self.block(async move {
            sqlx::query("UPDATE oplog SET undone = false WHERE repo_id = $1 AND batch_id = $2")
                .bind(repo_id)
                .bind(batch_id)
                .execute(pool.as_ref())
                .await
                .map_err(sqlx_err)?;
            Ok(())
        })?;
        let mut updated = batch.clone();
        for entry in &mut updated.entries {
            entry.undone = false;
        }
        Ok(updated)
    }
}

// ── Issue #62 regression: current-thread runtime panic ──────────────────────
#[cfg(test)]
mod current_thread_runtime_tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// Connection URL for the integration test: the CI `postgres-tests`
    /// job sets `DATABASE_URL` to the service container; everywhere else it
    /// is unset and we fall back to a deliberately-unreachable endpoint
    /// (port 1) so the call still round-trips through the runtime bridge
    /// without depending on a real database.
    fn test_database_url() -> String {
        std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://heddle-test@127.0.0.1:1/heddle_test".to_string())
    }

    /// Issue #62: `PgOpLogBackend`'s sync methods must not panic when the
    /// caller is on a `current_thread` Tokio runtime. The pre-fix
    /// `tokio::task::block_in_place(...)` path is only valid on a
    /// `multi_thread` runtime; on `current_thread` it panics with
    /// `"can call blocking only when running on the multi-threaded runtime"`.
    ///
    /// `#[ignore]`: the postgres integration suite only runs under the CI
    /// `postgres-tests` job (`cargo test --features postgres -- --ignored`)
    /// which provides a real `DATABASE_URL`. Without it the pool points at an
    /// unreachable endpoint, so the method returns `Err` rather than `Ok` —
    /// either way the contract under test is the *absence of a panic*.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "postgres integration: needs DATABASE_URL (CI postgres-tests job)"]
    async fn pg_oplog_methods_do_not_panic_on_current_thread_runtime() {
        // Short `acquire_timeout` keeps the test snappy when the endpoint is
        // unreachable: the future resolves to `Err(PoolTimedOut)` instead of
        // waiting for sqlx's 30 s default.
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy(&test_database_url())
            .expect("connect_lazy accepts the URL");
        let backend = PgOpLogBackend::new(Arc::new(pool), Uuid::new_v4());
        // `last()` is the cheapest read; the panic surfaces inside
        // `self.block(...)` regardless of which sync method we pick. Result
        // can be `Ok(None)` or `Err(...)` — only the absence of a panic
        // matters here.
        let _ = backend.last();
    }
}
