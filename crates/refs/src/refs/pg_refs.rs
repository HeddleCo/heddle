// SPDX-License-Identifier: Apache-2.0
//! Postgres-backed reference storage for the stateless server.

#![cfg(feature = "postgres")]

use std::sync::Arc;

use objects::{
    error::{HeddleError, Result},
    object::ChangeId,
};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use super::{CoreRefBackend, Head, RefBackend, RefExpectation, RefUpdate, resolve_refspec};

fn sqlx_err(e: sqlx::Error) -> HeddleError {
    HeddleError::Io(std::io::Error::other(e.to_string()))
}

#[derive(Clone)]
pub struct PgRefBackend {
    pool: Arc<PgPool>,
    repo_id: Uuid,
}

impl PgRefBackend {
    pub fn new(pool: Arc<PgPool>, repo_id: Uuid) -> Self {
        Self { pool, repo_id }
    }

    fn block<F, T>(&self, f: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>> + Send,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }

    fn id_to_bytes(id: &ChangeId) -> Vec<u8> {
        id.as_bytes().to_vec()
    }

    fn bytes_to_id(bytes: Vec<u8>) -> Result<ChangeId> {
        let arr: [u8; 16] = bytes
            .try_into()
            .map_err(|_| HeddleError::InvalidObject("invalid ChangeId bytes in database".into()))?;
        Ok(ChangeId::from_bytes(arr))
    }

    async fn get_ref_async(
        pool: &PgPool,
        repo_id: Uuid,
        name: &str,
        is_thread: bool,
    ) -> Result<Option<ChangeId>> {
        let row = sqlx::query(
            "SELECT change_id FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = $3",
        )
        .bind(repo_id)
        .bind(name)
        .bind(is_thread)
        .fetch_optional(pool)
        .await
        .map_err(sqlx_err)?;

        row.map(|r| {
            let bytes: Vec<u8> = r.try_get("change_id").map_err(sqlx_err)?;
            Self::bytes_to_id(bytes)
        })
        .transpose()
    }
}

impl CoreRefBackend for PgRefBackend {
    type Error = HeddleError;

    fn read_head(&self) -> Result<Head> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        self.block(async move {
            let maybe_row = sqlx::query("SELECT thread, change_id FROM heads WHERE repo_id = $1")
                .bind(repo_id)
                .fetch_optional(pool.as_ref())
                .await
                .map_err(sqlx_err)?;

            match maybe_row {
                None => Ok(Head::Attached {
                    thread: "main".to_string(),
                }),
                Some(r) => {
                    let thread: Option<String> = r.try_get("thread").map_err(sqlx_err)?;
                    let change_id: Option<Vec<u8>> = r.try_get("change_id").map_err(sqlx_err)?;
                    if let Some(t) = thread {
                        Ok(Head::Attached { thread: t })
                    } else if let Some(b) = change_id {
                        Ok(Head::Detached {
                            state: Self::bytes_to_id(b)?,
                        })
                    } else {
                        Ok(Head::Attached {
                            thread: "main".to_string(),
                        })
                    }
                }
            }
        })
    }

    fn write_head(&self, head: &Head) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let (thread, change_id): (Option<String>, Option<Vec<u8>>) = match head {
            Head::Attached { thread } => (Some(thread.clone()), None),
            Head::Detached { state } => (None, Some(Self::id_to_bytes(state))),
        };
        self.block(async move {
            sqlx::query(
                "INSERT INTO heads (repo_id, thread, change_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (repo_id)
                 DO UPDATE SET thread = EXCLUDED.thread, change_id = EXCLUDED.change_id",
            )
            .bind(repo_id)
            .bind(thread)
            .bind(change_id)
            .execute(pool.as_ref())
            .await
            .map_err(sqlx_err)?;
            Ok(())
        })
    }

    fn write_head_cas(&self, expected: RefExpectation<Head>, head: &Head) -> Result<()> {
        let current = self.read_head()?;
        match &expected {
            RefExpectation::Any => {}
            RefExpectation::Missing => {
                return Err(HeddleError::Conflict(
                    "HEAD cannot be missing on a Postgres backend".into(),
                ));
            }
            RefExpectation::Value(expected_head) => {
                if &current != expected_head {
                    return Err(HeddleError::Conflict(format!(
                        "HEAD CAS conflict: expected {:?}, found {:?}",
                        expected_head, current
                    )));
                }
            }
        }
        self.write_head(head)
    }

    fn get_thread(&self, name: &str) -> Result<Option<ChangeId>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { Self::get_ref_async(&pool, repo_id, &name, true).await })
    }
    fn set_thread(&self, name: &str, state: &ChangeId) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        let bytes = Self::id_to_bytes(state);
        self.block(async move { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, true, $3, NOW()) ON CONFLICT (repo_id, name) DO UPDATE SET change_id = EXCLUDED.change_id, updated_at = NOW()").bind(repo_id).bind(&name).bind(bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?; Ok(()) })
    }
    fn set_thread_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        let new_bytes = Self::id_to_bytes(state);
        self.block(async move { match expected { RefExpectation::Any => { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, true, $3, NOW()) ON CONFLICT (repo_id, name) DO UPDATE SET change_id = EXCLUDED.change_id, updated_at = NOW()").bind(repo_id).bind(&name).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?; } RefExpectation::Missing => { let n = sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, true, $3, NOW()) ON CONFLICT DO NOTHING").bind(repo_id).bind(&name).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(); if n == 0 { return Err(HeddleError::Conflict(format!("thread '{}' already exists", name))); } } RefExpectation::Value(old) => { let old_bytes = Self::id_to_bytes(&old); let n = sqlx::query("UPDATE refs SET change_id = $4, updated_at = NOW() WHERE repo_id = $1 AND name = $2 AND is_thread = true AND change_id = $3").bind(repo_id).bind(&name).bind(old_bytes).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(); if n == 0 { return Err(HeddleError::Conflict(format!("thread '{}' CAS conflict", name))); } } } Ok(()) })
    }
    fn delete_thread(&self, name: &str) -> Result<Option<ChangeId>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { let row = sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = true RETURNING change_id").bind(repo_id).bind(&name).fetch_optional(pool.as_ref()).await.map_err(sqlx_err)?; row.map(|r| { let bytes: Vec<u8> = r.try_get("change_id").map_err(sqlx_err)?; Self::bytes_to_id(bytes) }).transpose() })
    }
    fn delete_thread_cas(&self, name: &str, expected: RefExpectation<ChangeId>) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { let n = match expected { RefExpectation::Any | RefExpectation::Missing => sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = true").bind(repo_id).bind(&name).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(), RefExpectation::Value(old) => { let old_bytes = Self::id_to_bytes(&old); sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = true AND change_id = $3").bind(repo_id).bind(&name).bind(old_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected() } }; if n == 0 { Err(HeddleError::Conflict(format!("thread '{}' delete CAS conflict", name))) } else { Ok(()) } })
    }
    fn list_threads(&self) -> Result<Vec<String>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        self.block(async move {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM refs WHERE repo_id = $1 AND is_thread = true ORDER BY name",
            )
            .bind(repo_id)
            .fetch_all(pool.as_ref())
            .await
            .map_err(sqlx_err)
        })
    }
    fn get_marker(&self, name: &str) -> Result<Option<ChangeId>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { Self::get_ref_async(&pool, repo_id, &name, false).await })
    }
    fn create_marker(&self, name: &str, state: &ChangeId) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        let bytes = Self::id_to_bytes(state);
        self.block(async move { let n = sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, false, $3, NOW()) ON CONFLICT DO NOTHING").bind(repo_id).bind(&name).bind(bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(); if n == 0 { Err(HeddleError::Conflict(format!("marker '{}' already exists", name))) } else { Ok(()) } })
    }
    fn set_marker_cas(
        &self,
        name: &str,
        expected: RefExpectation<ChangeId>,
        state: &ChangeId,
    ) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        let new_bytes = Self::id_to_bytes(state);
        self.block(async move { match expected { RefExpectation::Any => { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, false, $3, NOW()) ON CONFLICT (repo_id, name) DO UPDATE SET change_id = EXCLUDED.change_id, updated_at = NOW()").bind(repo_id).bind(&name).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?; } RefExpectation::Missing => { let n = sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, false, $3, NOW()) ON CONFLICT DO NOTHING").bind(repo_id).bind(&name).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(); if n == 0 { return Err(HeddleError::Conflict(format!("marker '{}' already exists", name))); } } RefExpectation::Value(old) => { let old_bytes = Self::id_to_bytes(&old); let n = sqlx::query("UPDATE refs SET change_id = $4, updated_at = NOW() WHERE repo_id = $1 AND name = $2 AND is_thread = false AND change_id = $3").bind(repo_id).bind(&name).bind(old_bytes).bind(&new_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(); if n == 0 { return Err(HeddleError::Conflict(format!("marker '{}' CAS conflict", name))); } } } Ok(()) })
    }
    fn delete_marker(&self, name: &str) -> Result<Option<ChangeId>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { let row = sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = false RETURNING change_id").bind(repo_id).bind(&name).fetch_optional(pool.as_ref()).await.map_err(sqlx_err)?; row.map(|r| { let bytes: Vec<u8> = r.try_get("change_id").map_err(sqlx_err)?; Self::bytes_to_id(bytes) }).transpose() })
    }
    fn delete_marker_cas(&self, name: &str, expected: RefExpectation<ChangeId>) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let name = name.to_string();
        self.block(async move { let n = match expected { RefExpectation::Any | RefExpectation::Missing => sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = false").bind(repo_id).bind(&name).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected(), RefExpectation::Value(old) => { let old_bytes = Self::id_to_bytes(&old); sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = false AND change_id = $3").bind(repo_id).bind(&name).bind(old_bytes).execute(pool.as_ref()).await.map_err(sqlx_err)?.rows_affected() } }; if n == 0 { Err(HeddleError::Conflict(format!("marker '{}' delete CAS conflict", name))) } else { Ok(()) } })
    }
    fn list_markers(&self) -> Result<Vec<String>> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        self.block(async move {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM refs WHERE repo_id = $1 AND is_thread = false ORDER BY name",
            )
            .bind(repo_id)
            .fetch_all(pool.as_ref())
            .await
            .map_err(sqlx_err)
        })
    }
    fn update_refs(&self, updates: &[RefUpdate]) -> Result<()> {
        let pool = Arc::clone(&self.pool);
        let repo_id = self.repo_id;
        let updates = updates.to_vec();
        self.block(async move { let mut tx = pool.begin().await.map_err(sqlx_err)?; for update in &updates { match update { RefUpdate::Thread { name, expected, new } => match (expected, new) { (_, None) => { sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = true").bind(repo_id).bind(name).execute(&mut *tx).await.map_err(sqlx_err)?; } (RefExpectation::Missing, Some(state)) => { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, true, $3, NOW()) ON CONFLICT DO NOTHING").bind(repo_id).bind(name).bind(Self::id_to_bytes(state)).execute(&mut *tx).await.map_err(sqlx_err)?; } (RefExpectation::Value(old), Some(new_state)) => { sqlx::query("UPDATE refs SET change_id = $4, updated_at = NOW() WHERE repo_id = $1 AND name = $2 AND is_thread = true AND change_id = $3").bind(repo_id).bind(name).bind(Self::id_to_bytes(old)).bind(Self::id_to_bytes(new_state)).execute(&mut *tx).await.map_err(sqlx_err)?; } (_, Some(state)) => { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, true, $3, NOW()) ON CONFLICT (repo_id, name) DO UPDATE SET change_id = EXCLUDED.change_id, updated_at = NOW()").bind(repo_id).bind(name).bind(Self::id_to_bytes(state)).execute(&mut *tx).await.map_err(sqlx_err)?; } }, RefUpdate::Marker { name, expected: _, new } => match new { None => { sqlx::query("DELETE FROM refs WHERE repo_id = $1 AND name = $2 AND is_thread = false").bind(repo_id).bind(name).execute(&mut *tx).await.map_err(sqlx_err)?; } Some(state) => { sqlx::query("INSERT INTO refs (repo_id, name, is_thread, change_id, updated_at) VALUES ($1, $2, false, $3, NOW()) ON CONFLICT (repo_id, name) DO UPDATE SET change_id = EXCLUDED.change_id, updated_at = NOW()").bind(repo_id).bind(name).bind(Self::id_to_bytes(state)).execute(&mut *tx).await.map_err(sqlx_err)?; } }, RefUpdate::Head { new, .. } => { let (thread, change_id): (Option<String>, Option<Vec<u8>>) = match new { Head::Attached { thread } => (Some(thread.clone()), None), Head::Detached { state } => (None, Some(Self::id_to_bytes(state))), }; sqlx::query("INSERT INTO heads (repo_id, thread, change_id) VALUES ($1, $2, $3) ON CONFLICT (repo_id) DO UPDATE SET thread = EXCLUDED.thread, change_id = EXCLUDED.change_id").bind(repo_id).bind(thread).bind(change_id).execute(&mut *tx).await.map_err(sqlx_err)?; } } } tx.commit().await.map_err(sqlx_err)?; Ok(()) })
    }
    fn resolve(&self, refspec: &str) -> Result<Option<ChangeId>> {
        resolve_refspec(
            refspec,
            || self.read_head(),
            |name| self.get_thread(name),
            |name| self.get_marker(name),
        )
    }
}

impl RefBackend for PgRefBackend {
    fn get_remote_thread(&self, _remote: &str, _track: &str) -> Result<Option<ChangeId>> {
        Err(HeddleError::Conflict(
            "remote threading refs are not supported on the server backend".into(),
        ))
    }
    fn set_remote_thread(&self, _remote: &str, _track: &str, _state: &ChangeId) -> Result<()> {
        Err(HeddleError::Conflict(
            "remote threading refs are not supported on the server backend".into(),
        ))
    }
    fn delete_remote_thread(&self, _remote: &str, _track: &str) -> Result<Option<ChangeId>> {
        Err(HeddleError::Conflict(
            "remote threading refs are not supported on the server backend".into(),
        ))
    }
    fn list_remotes(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn list_remote_threads(&self, _remote: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

// ── Issue #62 regression: current-thread runtime panic ──────────────────────
#[cfg(test)]
mod current_thread_runtime_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    /// Issue #62: `PgRefBackend`'s sync methods must not panic when the
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
    async fn pg_refs_methods_do_not_panic_on_current_thread_runtime() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://heddle-test@127.0.0.1:1/heddle_test")
            .expect("connect_lazy accepts the URL");
        let backend = PgRefBackend::new(Arc::new(pool), Uuid::new_v4());
        // `read_head()` is the cheapest read; the panic surfaces inside
        // `self.block(...)` regardless of which sync method we pick. Result
        // can be `Ok(_)` or `Err(...)` — only the absence of a panic
        // matters here.
        let _ = backend.read_head();
    }
}