use std::path::Path;

use chrono::{SecondsFormat, Utc};
use objects::error::{HeddleError, Result};
use rusqlite::{Connection, OptionalExtension, ToSql, TransactionBehavior, params};

use super::{ActorPresence, ActorPresenceStatus, ActorPresenceStore, STALE_AGENT_TTL_DAYS};

pub(crate) fn initialize_schema(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE actor_presence (
      session_id TEXT PRIMARY KEY, status TEXT NOT NULL, started_at TEXT NOT NULL,
      terminal_at INTEGER, native_actor_key TEXT, native_instance_key TEXT,
      client_instance_id TEXT, heddle_session_id TEXT, path BLOB, payload BLOB NOT NULL);
      CREATE INDEX actor_presence_started ON actor_presence(started_at DESC,session_id);
      CREATE INDEX actor_presence_active ON actor_presence(status,started_at DESC,session_id);
      CREATE INDEX actor_presence_native ON actor_presence(native_actor_key,status,started_at DESC,session_id);
      CREATE INDEX actor_presence_native_history ON actor_presence(native_actor_key,started_at DESC,session_id);
      CREATE INDEX actor_presence_instance ON actor_presence(native_instance_key,path,status,started_at DESC,session_id);
      CREATE INDEX actor_presence_client ON actor_presence(client_instance_id,status,started_at DESC,session_id);
      CREATE INDEX actor_presence_session ON actor_presence(heddle_session_id,status,started_at DESC,session_id);
      CREATE INDEX actor_presence_path ON actor_presence(path,status,started_at DESC,session_id);
      CREATE INDEX actor_presence_terminal ON actor_presence(terminal_at) WHERE terminal_at IS NOT NULL;")
}

pub(super) fn error(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::Config(format!("actor presence metadata: {error}"))
}
pub(super) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(error(
            "session ID must contain lowercase alphanumeric characters or hyphens",
        ));
    }
    Ok(())
}
pub(super) fn path_key(path: &Path) -> Vec<u8> {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .as_os_str()
        .as_encoded_bytes()
        .to_vec()
}
pub(super) fn save(db: &Connection, entry: &ActorPresence) -> Result<()> {
    validate_id(&entry.session_id)?;
    let payload = serde_json::to_vec(entry).map_err(error)?;
    let terminal = (entry.status != ActorPresenceStatus::Active)
        .then(|| entry.completed_at.unwrap_or(entry.started_at).timestamp());
    db.execute("INSERT INTO actor_presence(session_id,status,started_at,terminal_at,native_actor_key,native_instance_key,client_instance_id,heddle_session_id,path,payload)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
        ON CONFLICT(session_id) DO UPDATE SET status=excluded.status,started_at=excluded.started_at,terminal_at=excluded.terminal_at,
        native_actor_key=excluded.native_actor_key,native_instance_key=excluded.native_instance_key,
        client_instance_id=excluded.client_instance_id,heddle_session_id=excluded.heddle_session_id,path=excluded.path,payload=excluded.payload",
        params![entry.session_id, entry.status.to_string(), entry.started_at.to_rfc3339_opts(SecondsFormat::Nanos, true), terminal,
            entry.native_actor_key,entry.native_instance_key,entry.client_instance_id,entry.heddle_session_id,
            entry.path.as_deref().map(path_key),payload]).map_err(error)?;
    Ok(())
}
pub(super) fn load(db: &Connection, id: &str) -> Result<Option<ActorPresence>> {
    let payload: Option<Vec<u8>> = db
        .query_row(
            "SELECT payload FROM actor_presence WHERE session_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(error)?;
    payload
        .map(|bytes| serde_json::from_slice(&bytes).map_err(error))
        .transpose()
}
pub(super) fn query(
    db: &Connection,
    predicate: &str,
    args: &[&dyn ToSql],
    current: bool,
) -> Result<Vec<ActorPresence>> {
    query_limit(db, predicate, args, current, false)
}
fn query_limit(
    db: &Connection,
    predicate: &str,
    args: &[&dyn ToSql],
    current: bool,
    first: bool,
) -> Result<Vec<ActorPresence>> {
    // Predicates are private compile-time SQL fragments; every caller value is bound.
    let cutoff = (Utc::now() - chrono::Duration::days(STALE_AGENT_TTL_DAYS)).timestamp();
    let ttl = if current {
        format!(" AND (terminal_at IS NULL OR terminal_at>{cutoff})")
    } else {
        String::new()
    };
    let limit = if first { " LIMIT 1" } else { "" };
    let mut statement = db.prepare(&format!("SELECT payload FROM actor_presence WHERE ({predicate}){ttl} ORDER BY started_at DESC,session_id{limit}"))
        .map_err(error)?;
    let rows = statement
        .query_map(args, |row| row.get::<_, Vec<u8>>(0))
        .map_err(error)?;
    rows.map(|row| serde_json::from_slice(&row.map_err(error)?).map_err(error))
        .collect()
}

impl ActorPresenceStore {
    pub(super) fn query(
        &self,
        predicate: &str,
        args: &[&dyn ToSql],
        current: bool,
    ) -> Result<Vec<ActorPresence>> {
        let Some(db) = crate::local_metadata::open_existing(&self.heddle_dir).map_err(error)?
        else {
            return Ok(Vec::new());
        };
        query(&db, predicate, args, current)
    }
    pub(super) fn first(
        &self,
        predicate: &str,
        args: &[&dyn ToSql],
        current: bool,
    ) -> Result<Option<ActorPresence>> {
        let Some(db) = crate::local_metadata::open_existing(&self.heddle_dir).map_err(error)?
        else {
            return Ok(None);
        };
        Ok(query_limit(&db, predicate, args, current, true)?
            .into_iter()
            .next())
    }
    pub(super) fn mutate<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        objects::fs_atomic::create_dir_all_durable(&self.heddle_dir)?;
        let mut db = crate::local_metadata::open(&self.heddle_dir).map_err(error)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let result = operation(&tx)?;
        // Cleanup work is bounded independently of accumulated terminal history.
        tx.execute("DELETE FROM actor_presence WHERE session_id IN
            (SELECT session_id FROM actor_presence WHERE terminal_at<=?1 ORDER BY terminal_at LIMIT 256)",
            [(Utc::now() - chrono::Duration::days(STALE_AGENT_TTL_DAYS)).timestamp()]).map_err(error)?;
        tx.commit().map_err(error)?;
        Ok(result)
    }
}
