//! Durable device run controls. Commands are requests until the actual harness
//! acknowledges them; receiving a command never pretends a process has paused.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1::{
    ControlRunRequest, PutRunPolicyRequest, RunPolicy, RunRecord, TimelineRecord,
};
use prost::Message;
use rusqlite::{Connection, OptionalExtension, params};
#[path = "device_run_permissions.rs"]
mod permissions;

#[derive(Clone, Debug)]
pub struct RunStore {
    path: PathBuf,
    // Keep WAL lifecycle independent of individual reads and hook invocations.
    _anchor: std::sync::Arc<std::sync::Mutex<Connection>>,
}
#[derive(Clone, Debug)]
pub enum RunObservation {
    Run(RunRecord),
    Timeline(TimelineRecord),
}
#[derive(Clone, Debug)]
pub struct RunControl {
    pub id: String,
    pub action: i32,
    pub instruction: String,
    pub principal: String,
}
impl RunStore {
    pub fn open(heddle_dir: &Path) -> Result<Self> {
        let path = heddle_dir.join("device-runs.sqlite3");
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, thread TEXT NOT NULL, record BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS run_harness_bindings (native_key TEXT PRIMARY KEY, run TEXT NOT NULL, active INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS run_timeline (run TEXT NOT NULL, position INTEGER NOT NULL, record BLOB NOT NULL, PRIMARY KEY(run,position));
            CREATE TABLE IF NOT EXISTS run_controls (sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, run TEXT NOT NULL, request BLOB NOT NULL, principal TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS pending_run_controls ON run_controls(run, done, sequence);
            CREATE TABLE IF NOT EXISTS run_permissions (run TEXT NOT NULL, id TEXT NOT NULL, digest BLOB NOT NULL, record BLOB NOT NULL, expires INTEGER NOT NULL, closed INTEGER NOT NULL DEFAULT 0, decision INTEGER, PRIMARY KEY(run,id));
            CREATE TABLE IF NOT EXISTS run_policies (spool TEXT PRIMARY KEY, policy BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS run_commands (id TEXT PRIMARY KEY, method TEXT NOT NULL, body BLOB NOT NULL, principal TEXT NOT NULL);")?;
        Ok(Self {
            path,
            _anchor: std::sync::Arc::new(std::sync::Mutex::new(connection)),
        })
    }
    /// Hook reads never create a database or execute schema statements.
    pub fn open_existing(heddle_dir: &Path) -> Result<Option<Self>> {
        let path = heddle_dir.join("device-runs.sqlite3");
        if !path.try_exists()? {
            return Ok(None);
        }
        let connection =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Some(Self {
            path,
            _anchor: std::sync::Arc::new(std::sync::Mutex::new(connection)),
        }))
    }
    pub fn bind_harness(&self, native_key: &str, run: &str, active: bool) -> Result<()> {
        if native_key.is_empty() || native_key.len() > 512 {
            bail!("invalid harness identity");
        }
        valid_id(run)?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if load_run(&tx, run)?.is_none() {
            bail!("harness run unavailable");
        }
        let existing: Option<(String, bool)> = tx
            .query_row(
                "SELECT run,active FROM run_harness_bindings WHERE native_key=?1",
                [native_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|(previous, active)| *active && previous != run)
        {
            bail!("harness identity is already bound to another active run");
        }
        tx.execute("INSERT INTO run_harness_bindings(native_key,run,active) VALUES(?1,?2,?3) ON CONFLICT(native_key) DO UPDATE SET run=excluded.run,active=excluded.active WHERE run_harness_bindings.run!=excluded.run OR run_harness_bindings.active!=excluded.active", params![native_key,run,active])?;
        tx.commit()?;
        Ok(())
    }
    /// Indexed physical-checkout-local lookup; never scans other agents' sessions.
    pub fn harness_run(&self, native_key: &str) -> Result<Option<String>> {
        if native_key.is_empty() || native_key.len() > 512 {
            bail!("invalid harness identity");
        }
        Ok(self
            .connection()?
            .query_row(
                "SELECT run FROM run_harness_bindings WHERE native_key=?1 AND active=1",
                [native_key],
                |row| row.get(0),
            )
            .optional()?)
    }
    fn committed(&self) -> Result<()> {
        objects::fs_atomic::write_file_atomic_secret(
            &self.path.with_extension("sqlite3.changed"),
            uuid::Uuid::now_v7().as_bytes(),
        )?;
        Ok(())
    }
    fn connection(&self) -> Result<Connection> {
        let connection =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }
    pub fn put_run(&self, mut run: RunRecord) -> Result<RunRecord> {
        let id = run
            .r#ref
            .as_ref()
            .context("run reference required")?
            .id
            .clone();
        valid_id(&id)?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        permissions::populate(&tx, &mut run)?;
        run.version.clear();
        run.version = blake3::hash(&run.encode_to_vec()).as_bytes().to_vec();
        if run.encode_to_vec().len() > 240 * 1024 {
            bail!("run record exceeds observation frame budget");
        }
        tx.execute("INSERT INTO runs(id,thread,record) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET thread=excluded.thread,record=excluded.record WHERE runs.record!=excluded.record",params![id,run.thread.as_ref().and_then(|thread|thread.id.as_ref()).map(|id|hex::encode(&id.value)).unwrap_or_default(),run.encode_to_vec()])?;
        tx.commit()?;
        // Explicit idempotent retry can repair a prior post-commit marker failure.
        self.committed()?;
        Ok(run)
    }
    pub fn run(&self, id: &str) -> Result<Option<RunRecord>> {
        load_run(&self.connection()?, id)
    }
    /// Stable keyset page; the caller's stream budget bounds each page.
    pub fn page(&self, after: &str, limit: usize) -> Result<Vec<RunRecord>> {
        if limit == 0 || limit > 1024 {
            bail!("invalid run page budget");
        }
        let connection = self.connection()?;
        let mut query =
            connection.prepare("SELECT record FROM runs WHERE id>?1 ORDER BY id LIMIT ?2")?;
        query
            .query_map(params![after, limit as i64], |row| row.get::<_, Vec<u8>>(0))?
            .map(|value| Ok(permissions::project(RunRecord::decode(value?.as_slice())?)))
            .collect()
    }
    pub fn last_timeline_position(&self, run_id: &str) -> Result<Option<u64>> {
        let position: Option<i64> = self.connection()?.query_row(
            "SELECT MAX(position) FROM run_timeline WHERE run=?1",
            [run_id],
            |row| row.get(0),
        )?;
        position
            .map(|value| u64::try_from(value).context("invalid stored timeline position"))
            .transpose()
    }
    /// Persist an immutable, already scrubbed harness event. Exact replay is a no-op.
    pub fn put_timeline(&self, record: &TimelineRecord) -> Result<()> {
        let run = record.run.as_ref().context("timeline run required")?;
        let reference = record
            .r#ref
            .as_ref()
            .context("timeline reference required")?;
        valid_id(&reference.id)?;
        if reference.spool != run.spool
            || record.position > i64::MAX as u64
            || record.encode_to_vec().len() > 256 * 1024
        {
            bail!("invalid timeline scope or size");
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if load_run(&tx, &run.id)?.is_none_or(|stored| stored.r#ref.as_ref() != Some(run)) {
            bail!("timeline run unavailable");
        }
        let body = record.encode_to_vec();
        tx.execute("INSERT INTO run_timeline(run,position,record) VALUES (?1,?2,?3) ON CONFLICT(run,position) DO NOTHING", params![run.id,record.position as i64,body])?;
        let stored: Vec<u8> = tx.query_row(
            "SELECT record FROM run_timeline WHERE run=?1 AND position=?2",
            params![run.id, record.position as i64],
            |row| row.get(0),
        )?;
        if stored != body {
            bail!("timeline position reused with different event");
        }
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    /// One bounded keyset for run summaries and their timeline events. Filters
    /// apply before LIMIT; a long unrelated run never hides a requested run.
    pub fn observation_page(
        &self,
        after: &str,
        limit: usize,
        run_ids: &[String],
        thread_ids: &[Vec<u8>],
        timeline: bool,
    ) -> Result<Vec<(String, RunObservation)>> {
        if limit == 0 || limit > 1025 || run_ids.len() > 1024 || thread_ids.len() > 1024 {
            bail!("invalid run observation budget");
        }
        let connection = self.connection()?;
        // The bounded id list is built from the caller's explicit filters. An
        // empty filter is represented directly, without loading the whole map.
        let run_filter = serde_json::to_string(run_ids)?;
        let thread_filter =
            serde_json::to_string(&thread_ids.iter().map(hex::encode).collect::<Vec<_>>())?;
        let mut query = connection.prepare("WITH candidates AS (
            SELECT 'r:'||id AS key,0 AS kind,record,id AS run FROM runs
            UNION ALL SELECT 't:'||run||':'||printf('%020d',position),1,record,run FROM run_timeline WHERE ?3
        ) SELECT c.key,c.kind,c.record,r.record FROM candidates c JOIN runs r ON r.id=c.run
        WHERE c.key>?1 AND (?4='[]' OR c.run IN (SELECT value FROM json_each(?4))) AND (?5='[]' OR r.thread IN (SELECT value FROM json_each(?5))) ORDER BY c.key LIMIT ?2")?;
        let rows = query.query_map(
            params![after, limit as i64, timeline, run_filter, thread_filter],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )?;
        let mut output = Vec::new();
        for row in rows {
            let (key, kind, body, run) = row?;
            let run = permissions::project(RunRecord::decode(run.as_slice())?);
            if !thread_ids.is_empty()
                && !run.thread.as_ref().is_some_and(|thread| {
                    thread
                        .id
                        .as_ref()
                        .is_some_and(|id| thread_ids.contains(&id.value))
                })
            {
                continue;
            }
            output.push((
                key,
                if kind == 0 {
                    RunObservation::Run(run)
                } else {
                    RunObservation::Timeline(TimelineRecord::decode(body.as_slice())?)
                },
            ));
            if output.len() == limit {
                break;
            }
        }
        Ok(output)
    }
    pub fn enqueue_control(&self, request: &ControlRunRequest, principal: &str) -> Result<()> {
        use api::heddle::api::v2alpha1::control_run_request::Action;
        valid_id(&request.client_operation_id)?;
        let reference = request.run.as_ref().context("run required")?;
        if principal.is_empty()
            || !matches!(
                Action::try_from(request.action),
                Ok(Action::Pause | Action::Resume | Action::Stop | Action::Steer)
            )
            || (request.action == Action::Steer as i32 && request.instruction.trim().is_empty())
            || request.instruction.len() > 64 * 1024
        {
            bail!("invalid run control");
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let body = request.encode_to_vec();
        if command_replay(
            &tx,
            &request.client_operation_id,
            "control",
            &body,
            principal,
        )? {
            return self.committed();
        }
        let run = load_run(&tx, &reference.id)?.context("run unavailable")?;
        if run.r#ref.as_ref() != Some(reference)
            || run.version != request.expected_version
            || request.expected_version.is_empty()
        {
            bail!("run version changed");
        }
        if !run.supported_controls.contains(&request.action) {
            bail!("harness does not support this run control");
        }
        tx.execute(
            "INSERT INTO run_controls(id,run,request,principal) VALUES (?1,?2,?3,?4)",
            params![request.client_operation_id, reference.id, body, principal],
        )?;
        record_command(
            &tx,
            &request.client_operation_id,
            "control",
            &body,
            principal,
        )?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    pub fn pending_controls(&self, run_id: &str, limit: usize) -> Result<Vec<RunControl>> {
        if limit == 0 || limit > 1024 {
            bail!("invalid control page budget");
        }
        let connection = self.connection()?;
        let mut query = connection.prepare("SELECT request,principal FROM run_controls WHERE run=?1 AND done=0 ORDER BY sequence LIMIT ?2")?;
        query
            .query_map(params![run_id, limit as i64], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (bytes, principal) = row?;
                let request = ControlRunRequest::decode(bytes.as_slice())?;
                Ok(RunControl {
                    id: request.client_operation_id,
                    action: request.action,
                    instruction: request.instruction,
                    principal,
                })
            })
            .collect()
    }
    /// A delivered hook response is not an observed process state transition.
    pub fn acknowledge_control_delivery(&self, command_id: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (run, done): (String, bool) = tx.query_row(
            "SELECT run,done FROM run_controls WHERE id=?1",
            [command_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if done {
            return self.committed();
        }
        let next: String = tx.query_row(
            "SELECT id FROM run_controls WHERE run=?1 AND done=0 ORDER BY sequence LIMIT 1",
            [run],
            |row| row.get(0),
        )?;
        if next != command_id {
            bail!("run controls must be acknowledged in request order");
        }
        tx.execute("UPDATE run_controls SET done=1 WHERE id=?1", [command_id])?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    pub fn complete_control(&self, command_id: &str, updated: &RunRecord) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let run_id: String = tx.query_row(
            "SELECT run FROM run_controls WHERE id=?1",
            [command_id],
            |row| row.get(0),
        )?;
        if updated
            .r#ref
            .as_ref()
            .is_none_or(|reference| reference.id != run_id)
        {
            bail!("control acknowledgment targets another run");
        }
        let done: bool = tx.query_row(
            "SELECT done FROM run_controls WHERE id=?1",
            [command_id],
            |row| row.get(0),
        )?;
        if done {
            return self.committed();
        }
        let next: String = tx.query_row(
            "SELECT id FROM run_controls WHERE run=?1 AND done=0 ORDER BY sequence LIMIT 1",
            [&run_id],
            |row| row.get(0),
        )?;
        if next != command_id {
            bail!("run controls must be acknowledged in request order");
        }
        let previous = load_run(&tx, &run_id)?.context("run disappeared")?;
        if previous.version != updated.version {
            bail!("run execution acknowledgment has stale observed version");
        }
        if previous.r#ref != updated.r#ref
            || previous.thread != updated.thread
            || previous.checkout != updated.checkout
            || previous.principal_id != updated.principal_id
        {
            bail!("run acknowledgment changes identity");
        }
        let mut updated = updated.clone();
        permissions::populate(&tx, &mut updated)?;
        updated.version.clear();
        updated.version = blake3::hash(&updated.encode_to_vec()).as_bytes().to_vec();
        tx.execute(
            "UPDATE runs SET record=?2 WHERE id=?1",
            params![run_id, updated.encode_to_vec()],
        )?;
        tx.execute("UPDATE run_controls SET done=1 WHERE id=?1", [command_id])?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    pub fn policy(&self, spool: &str) -> Result<Option<RunPolicy>> {
        self.connection()?
            .query_row(
                "SELECT policy FROM run_policies WHERE spool=?1",
                [spool],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| Ok(RunPolicy::decode(bytes.as_slice())?))
            .transpose()
    }
    pub fn put_policy(&self, request: &PutRunPolicyRequest, principal: &str) -> Result<()> {
        valid_id(&request.client_operation_id)?;
        let mut policy = request.policy.clone().context("run policy required")?;
        let spool = policy
            .spool
            .as_ref()
            .context("policy spool required")?
            .id
            .clone();
        valid_id(&spool)?;
        if !policy.retain_raw && policy.raw_retention_seconds != 0 {
            bail!("raw retention requires opt-in");
        }
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let body = request.encode_to_vec();
        if command_replay(
            &tx,
            &request.client_operation_id,
            "policy",
            &body,
            principal,
        )? {
            return self.committed();
        }
        let old: Option<Vec<u8>> = tx
            .query_row(
                "SELECT policy FROM run_policies WHERE spool=?1",
                [&spool],
                |row| row.get(0),
            )
            .optional()?;
        let version = old
            .map(|bytes| RunPolicy::decode(bytes.as_slice()))
            .transpose()?
            .map(|policy| policy.version)
            .unwrap_or_default();
        if version != request.expected_version {
            bail!("run policy version changed");
        }
        policy.version.clear();
        policy.version = blake3::hash(&policy.encode_to_vec()).as_bytes().to_vec();
        tx.execute("INSERT INTO run_policies(spool,policy) VALUES (?1,?2) ON CONFLICT(spool) DO UPDATE SET policy=excluded.policy",params![spool,policy.encode_to_vec()])?;
        record_command(
            &tx,
            &request.client_operation_id,
            "policy",
            &body,
            principal,
        )?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
}
fn load_run(connection: &Connection, id: &str) -> Result<Option<RunRecord>> {
    connection
        .query_row("SELECT record FROM runs WHERE id=?1", [id], |row| {
            row.get::<_, Vec<u8>>(0)
        })
        .optional()?
        .map(|bytes| Ok(permissions::project(RunRecord::decode(bytes.as_slice())?)))
        .transpose()
}
fn command_replay(
    connection: &Connection,
    id: &str,
    method: &str,
    body: &[u8],
    principal: &str,
) -> Result<bool> {
    let old: Option<(String, Vec<u8>, String)> = connection
        .query_row(
            "SELECT method,body,principal FROM run_commands WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match old {
        None => Ok(false),
        Some(old) if old == (method.to_owned(), body.to_vec(), principal.to_owned()) => Ok(true),
        Some(_) => bail!("operation id reused with different signed inputs"),
    }
}
fn record_command(
    connection: &Connection,
    id: &str,
    method: &str,
    body: &[u8],
    principal: &str,
) -> Result<()> {
    connection.execute(
        "INSERT INTO run_commands(id,method,body,principal) VALUES (?1,?2,?3,?4)",
        params![id, method, body, principal],
    )?;
    Ok(())
}
fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.contains('/') || id.contains('\\') {
        bail!("invalid device record identifier");
    }
    Ok(())
}

#[cfg(test)]
#[path = "device_runs_tests.rs"]
mod tests;
