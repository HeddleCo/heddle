// SPDX-License-Identifier: Apache-2.0
//! Postgres-backed operation log for the stateless server.
//!
//! Each operation is a row in the `oplog` table. Batches share a `batch_id`.
//! Undo/redo state is tracked with the `undone` column.

#![cfg(feature = "postgres")]

use std::sync::Arc;

use chrono::{DateTime, Utc};
use objects::error::{HeddleError, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{
    oplog_backend::OpLogBackend,
    oplog_types::{OpBatch, OpEntry, OpRecord},
};

fn sqlx_err(e: sqlx::Error) -> HeddleError {
    HeddleError::Io(std::io::Error::other(e.to_string()))
}

/// Postgres-backed operation log backend for the stateless server.
#[derive(Clone)]
pub struct PgOpLogBackend {
    pool: Arc<PgPool>,
    repo_id: Uuid,
}

impl PgOpLogBackend {
    pub fn new(pool: Arc<PgPool>, repo_id: Uuid) -> Self {
        Self { pool, repo_id }
    }

    fn block<F, T>(&self, f: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
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
            actor: objects::object::Principal::new("<unknown>", ""),
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
}

impl OpLogBackend for PgOpLogBackend {
    fn record_batch(&self, operations: Vec<OpRecord>) -> Result<Vec<u64>> {
        self.record_batch_scoped(operations, None)
    }

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

    fn recent_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.recent_batches_scoped(count, None)
    }

    fn recent_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let scope = scope.map(str::to_string);
        self.block(async move {
            let batch_ids: Vec<i64> = if let Some(scope) = scope.as_deref() {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog WHERE repo_id = $1 AND scope = $2
                     ORDER BY batch_id DESC LIMIT $3",
                )
                .bind(repo_id)
                .bind(scope)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog WHERE repo_id = $1
                     ORDER BY batch_id DESC LIMIT $2",
                )
                .bind(repo_id)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            };
            Self::fetch_batches_by_ids(&pool, repo_id, &batch_ids, false).await
        })
    }

    fn undo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.undo_batches_scoped(count, None)
    }

    fn undo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let scope = scope.map(str::to_string);
        self.block(async move {
            let batch_ids: Vec<i64> = if let Some(scope) = scope.as_deref() {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog
                     WHERE repo_id = $1 AND undone = false AND scope = $2
                     ORDER BY batch_id DESC LIMIT $3",
                )
                .bind(repo_id)
                .bind(scope)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog
                     WHERE repo_id = $1 AND undone = false
                     ORDER BY batch_id DESC LIMIT $2",
                )
                .bind(repo_id)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            };
            Self::fetch_batches_by_ids(&pool, repo_id, &batch_ids, false).await
        })
    }

    fn redo_batches(&self, count: usize) -> Result<Vec<OpBatch>> {
        self.redo_batches_scoped(count, None)
    }

    fn redo_batches_scoped(&self, count: usize, scope: Option<&str>) -> Result<Vec<OpBatch>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let scope = scope.map(str::to_string);
        self.block(async move {
            let batch_ids: Vec<i64> = if let Some(scope) = scope.as_deref() {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog
                     WHERE repo_id = $1 AND undone = true AND scope = $2
                     ORDER BY batch_id ASC LIMIT $3",
                )
                .bind(repo_id)
                .bind(scope)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            } else {
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT batch_id FROM oplog
                     WHERE repo_id = $1 AND undone = true
                     ORDER BY batch_id ASC LIMIT $2",
                )
                .bind(repo_id)
                .bind(count as i64)
                .fetch_all(pool.as_ref())
                .await
                .map_err(sqlx_err)?
            };
            Self::fetch_batches_by_ids(&pool, repo_id, &batch_ids, true).await
        })
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
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// Issue #62: `PgOpLogBackend`'s sync methods must not panic when the
    /// caller is on a `current_thread` Tokio runtime. The pre-fix
    /// `tokio::task::block_in_place(...)` path is only valid on a
    /// `multi_thread` runtime; on `current_thread` it panics with
    /// `"can call blocking only when running on the multi-threaded runtime"`.
    ///
    /// The pool is built with `connect_lazy` — no real database is required.
    /// Method calls return an `Err` when the bridged future fails to
    /// connect, which is fine: this test only asserts that the call returns
    /// a `Result` instead of panicking.
    #[tokio::test(flavor = "current_thread")]
    async fn pg_oplog_methods_do_not_panic_on_current_thread_runtime() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://heddle-test@127.0.0.1:1/heddle_test")
            .expect("connect_lazy accepts the URL");
        let backend = PgOpLogBackend::new(Arc::new(pool), Uuid::new_v4());
        // `last()` is the cheapest read; the panic surfaces inside
        // `self.block(...)` regardless of which sync method we pick. Result
        // can be `Ok(None)` or `Err(...)` — only the absence of a panic
        // matters here.
        let _ = backend.last();
    }
}