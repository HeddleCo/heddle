// SPDX-License-Identifier: Apache-2.0
//! Idempotency dedup store for `client_operation_id`.
//!
//! Every state-changing CLI verb and hosted method accepts an optional
//! `client_operation_id` (UUID v4). The first time the server sees an id it
//! processes the request and persists `(operation_id, request_hash, response)`.
//! If the same id arrives again for the same verb with the same body hash, the
//! server returns the cached response bit-identical without re-executing. If the
//! body or verb differs, the server returns `FailedPrecondition` so the caller
//! can detect the bug.
//!
//! Local repositories use the shared metadata SQLite database. Bootstrap
//! commands use an explicit separate database before a repository exists.
//! Completed receipts retain seven days by default; pending reservations require
//! explicit cancellation and are never expired underneath running work.

pub mod observation;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use objects::{
    error::{HeddleError, Result},
    object::OperationId,
    sync::LockExt,
};
use oplog::IsolationKey;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::{
    Repository,
    atomic::{AtomicMutation, Compensator, EagerMutation, StagedCommit, Tx, execute},
};

const COMPACTION_BATCH: usize = 256;
/// Default retention for completed local receipts. Pending work does not expire.
pub const DEFAULT_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// One persisted dedup entry. Identity is `(operation_id, verb)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupEntry {
    pub operation_id: OperationId,
    /// Hosted method name or CLI verb name, including the replay encoding
    /// generation when relevant. Operation IDs remain unique across the store;
    /// reusing one under a different verb is a conflict.
    pub verb: String,
    /// BLAKE3-256 of the request body bytes. The caller is responsible for
    /// choosing a deterministic encoding and including its generation in the
    /// verb whenever an encoding change would make cached data incompatible.
    pub request_hash: [u8; 32],
    /// Cached response bytes in the caller-owned encoding for this verb.
    /// Empty (`Vec::new()`) when [`pending`](Self::pending) is `true` —
    /// i.e. the slot is reserved but the response hasn't been recorded yet.
    pub response: Vec<u8>,
    /// Unix epoch seconds when this entry was created. Used by compaction.
    pub created_at_secs: i64,
    /// `true` when the entry is a reservation written by
    /// [`OperationDedupStore::reserve`] but not yet finalised by
    /// [`OperationDedupStore::record`]. Concurrent retries with the same
    /// `(operation_id, verb)` see [`DedupOutcome::InFlight`] while the
    /// reservation is held. Cleared by `record` (when the response is
    /// persisted) or [`OperationDedupStore::cancel`] (on execute failure).
    ///
    pub pending: bool,
}

/// Result of a [`OperationDedupStore::reserve`] call.
///
/// - [`DedupOutcome::Reserved`]: this id has not been seen, and the store
///   has atomically claimed the slot for the caller. The caller MUST
///   either complete the request via [`OperationDedupStore::record`] or
///   release the reservation via [`OperationDedupStore::cancel`]. While
///   the reservation is held, concurrent identical requests see
///   [`DedupOutcome::InFlight`].
/// - [`DedupOutcome::Replay`]: a completed entry exists with a matching
///   body hash; the cached response is returned and the request must
///   *not* be re-executed.
/// - [`DedupOutcome::InFlight`]: a reservation for the same
///   `(operation_id, verb)` is currently held by another caller (with
///   the same body hash). The caller should surface a transient error
///   (`Status::aborted`) so the client can retry once the original
///   completes.
/// - [`DedupOutcome::Conflict`]: same id, different body. Caller should
///   surface a `FailedPrecondition` to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupOutcome {
    Reserved,
    Replay { response: Vec<u8> },
    InFlight,
    Conflict,
}

/// Safe-to-report metadata for an existing op-id slot. This deliberately
/// omits cached response bytes; callers use it to explain conflicts without
/// leaking command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupConflictMetadata {
    pub operation_id: OperationId,
    pub verb: String,
    pub request_hash: [u8; 32],
    pub created_at_secs: i64,
    pub pending: bool,
}

#[derive(Clone)]
pub struct ReserveOpIdClaim {
    outcome: Arc<Mutex<Option<DedupOutcome>>>,
}

impl ReserveOpIdClaim {
    fn new() -> Self {
        Self {
            outcome: Arc::new(Mutex::new(None)),
        }
    }

    fn set(&self, outcome: DedupOutcome) {
        *self.outcome.lock_or_poisoned() = Some(outcome);
    }

    fn into_outcome(self) -> Result<DedupOutcome> {
        self.outcome
            .lock_or_poisoned()
            .take()
            .ok_or_else(|| HeddleError::Conflict("eager op-id reserve produced no outcome".into()))
    }
}

/// Eager op-id reservation for use inside an existing [`Tx`].
///
/// The reservation is written in [`EagerMutation::commit_eager`], not
/// [`AtomicMutation::apply`], so it is cross-process visible before the outer
/// transaction can continue. The compensator is intentionally a no-op: once an
/// op-id has been handed out, an unrelated outer abort must not make that
/// unique reservation disappear.
pub struct ReserveOpId {
    store: Arc<OperationDedupStore>,
    operation_id: OperationId,
    verb: String,
    request_hash: [u8; 32],
    claim: ReserveOpIdClaim,
}

impl ReserveOpId {
    pub fn new(
        store: Arc<OperationDedupStore>,
        operation_id: OperationId,
        verb: impl Into<String>,
        request_hash: [u8; 32],
    ) -> Self {
        Self {
            store,
            operation_id,
            verb: verb.into(),
            request_hash,
            claim: ReserveOpIdClaim::new(),
        }
    }
}

impl AtomicMutation for ReserveOpId {
    type Output = ReserveOpIdClaim;

    fn transaction_id(&self) -> String {
        reserve_transaction_id(self.operation_id, &self.verb, self.request_hash)
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        Ok(BTreeSet::new())
    }

    fn apply(&mut self, _tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>> {
        Ok(StagedCommit::pure(self.claim.clone()))
    }
}

impl EagerMutation for ReserveOpId {
    fn commit_eager(&mut self, _tx: &mut Tx<'_>) -> Result<Compensator> {
        let outcome = self
            .store
            .reserve(self.operation_id, &self.verb, self.request_hash)?;
        self.claim.set(outcome);
        Ok(Compensator::new(|| Ok(())))
    }
}

struct ReserveOpIdTransaction {
    store: Arc<OperationDedupStore>,
    operation_id: OperationId,
    verb: String,
    request_hash: [u8; 32],
}

impl ReserveOpIdTransaction {
    fn new(
        store: Arc<OperationDedupStore>,
        operation_id: OperationId,
        verb: impl Into<String>,
        request_hash: [u8; 32],
    ) -> Self {
        Self {
            store,
            operation_id,
            verb: verb.into(),
            request_hash,
        }
    }
}

impl AtomicMutation for ReserveOpIdTransaction {
    type Output = DedupOutcome;

    fn transaction_id(&self) -> String {
        reserve_transaction_id(self.operation_id, &self.verb, self.request_hash)
    }

    fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
        Ok(BTreeSet::new())
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<Self::Output>> {
        let claim = tx.enroll_eager(ReserveOpId::new(
            Arc::clone(&self.store),
            self.operation_id,
            self.verb.clone(),
            self.request_hash,
        ))?;
        Ok(StagedCommit::pure(claim.into_outcome()?))
    }
}

pub fn reserve_operation_id_eager(
    repo: &Repository,
    store: Arc<OperationDedupStore>,
    operation_id: OperationId,
    verb: impl Into<String>,
    request_hash: [u8; 32],
) -> Result<DedupOutcome> {
    execute(
        repo,
        ReserveOpIdTransaction::new(store, operation_id, verb, request_hash),
    )
}

fn reserve_transaction_id(operation_id: OperationId, verb: &str, request_hash: [u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut hash = String::with_capacity(64);
    for byte in request_hash {
        let _ = write!(&mut hash, "{byte:02x}");
    }
    format!("op-id-reserve/{verb}/{operation_id}/{hash}")
}

/// SQLite serializes short reservation transitions across handles/processes.
/// No transaction remains open while the caller executes its command.
pub struct OperationDedupStore {
    change_marker: Option<PathBuf>,
    namespace: String,
    connection: Mutex<Connection>,
}

pub(crate) fn initialize_schema(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE operation_receipts (
      namespace TEXT NOT NULL DEFAULT '', operation_id TEXT NOT NULL, record_id BLOB UNIQUE CHECK(record_id IS NULL OR length(record_id)=32), verb TEXT NOT NULL,
      request_hash BLOB NOT NULL CHECK(length(request_hash)=32), response BLOB NOT NULL,
      created_at INTEGER NOT NULL, pending INTEGER NOT NULL CHECK(pending IN(0,1)),
      PRIMARY KEY(namespace,operation_id), CHECK(pending=0 OR length(response)=0));
      CREATE INDEX operation_receipts_namespace ON operation_receipts(namespace,created_at,operation_id);
      CREATE INDEX operation_receipts_namespace_record ON operation_receipts(namespace,record_id);
      CREATE INDEX operation_receipts_completed ON operation_receipts(created_at,operation_id) WHERE pending=0;")
}
fn database_error(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::InvalidObject(format!("operation receipt database: {error}"))
}
impl OperationDedupStore {
    pub fn open(heddle_dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            change_marker: Some(
                heddle_dir
                    .as_ref()
                    .join(crate::local_metadata::DATABASE_NAME)
                    .with_extension("sqlite3.changed"),
            ),
            namespace: String::new(),
            connection: Mutex::new(
                crate::local_metadata::open(heddle_dir.as_ref()).map_err(database_error)?,
            ),
        })
    }
    /// RPC callers supply their authenticated stable principal/agent namespace.
    /// Empty is reserved for ordinary local CLI receipts and is never exposed
    /// by the device operation observer.
    pub fn open_scoped(heddle_dir: impl AsRef<Path>, namespace: &str) -> Result<Self> {
        if namespace.is_empty() || namespace.len() > 1024 || namespace.contains('\0') {
            return Err(database_error("invalid authenticated operation namespace"));
        }
        let mut store = Self::open(heddle_dir)?;
        store.namespace = namespace.to_owned();
        Ok(store)
    }
    /// Explicit scope for init/clone receipts, never implicitly selected by a
    /// missing repository. No legacy file is read or written.
    pub fn open_bootstrap(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        objects::fs_atomic::create_private_dir_all(directory)?;
        let path = directory.join("operation-receipts.sqlite3");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut connection = Connection::open(path).map_err(database_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(database_error)?;
        let mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(database_error)?;
        if mode != "wal" {
            return Err(database_error("WAL mode unavailable"));
        }
        connection
            .execute_batch("PRAGMA synchronous=FULL;")
            .map_err(database_error)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let version: i64 = tx
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(database_error)?;
        match version {
            0 => {
                initialize_schema(&tx).map_err(database_error)?;
                tx.pragma_update(None, "user_version", 1)
                    .map_err(database_error)?;
            }
            1 => {}
            other => {
                return Err(database_error(format!(
                    "unsupported bootstrap schema {other}"
                )));
            }
        }
        tx.commit().map_err(database_error)?;
        Ok(Self {
            change_marker: None,
            namespace: String::new(),
            connection: Mutex::new(connection),
        })
    }
    fn notify_committed(&self) -> Result<()> {
        if let Some(path) = &self.change_marker {
            objects::fs_atomic::write_file_atomic_secret(path, uuid::Uuid::now_v7().as_bytes())?;
        }
        Ok(())
    }
    fn record_key(&self, id: OperationId) -> Option<Vec<u8>> {
        (!self.namespace.is_empty())
            .then(|| receipt_record_key(&self.namespace, id).as_bytes().to_vec())
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| database_error("connection guard poisoned"))
    }
    pub fn reserve(
        &self,
        operation_id: OperationId,
        verb: &str,
        request_hash: [u8; 32],
    ) -> Result<DedupOutcome> {
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        prune(&tx, now_secs().saturating_sub(DEFAULT_RETENTION_SECS))?;
        // Direct expiry of the requested completed key remains correct even if
        // more than a cleanup batch has accumulated since the last command.
        tx.execute(
            "DELETE FROM operation_receipts WHERE namespace=?3 AND operation_id=?1 AND pending=0 AND created_at<?2",
            params![
                operation_id.to_string(),
                now_secs().saturating_sub(DEFAULT_RETENTION_SECS),
                self.namespace
            ],
        )
        .map_err(database_error)?;
        let prior = entry(&tx, &self.namespace, operation_id)?;
        let outcome = match prior {
            Some(prior) if prior.verb != verb || prior.request_hash != request_hash => {
                DedupOutcome::Conflict
            }
            Some(prior) if prior.pending => DedupOutcome::InFlight,
            Some(prior) => DedupOutcome::Replay {
                response: prior.response,
            },
            None => {
                tx.execute(
                    "INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?5,?1,?6,?2,?3,x'',?4,1)",
                    params![
                        operation_id.to_string(),
                        verb,
                        request_hash.as_slice(),
                        now_secs(),
                        self.namespace,
                        self.record_key(operation_id)
                    ],
                )
                .map_err(database_error)?;
                DedupOutcome::Reserved
            }
        };
        tx.commit().map_err(database_error)?;
        drop(connection);
        if outcome == DedupOutcome::Reserved {
            self.notify_committed()?;
        }
        Ok(outcome)
    }
    /// Finalization preserves the first completed response and timestamp.
    /// Direct recording is supported, but may not overwrite a conflicting slot.
    pub fn record(
        &self,
        operation_id: OperationId,
        verb: &str,
        request_hash: [u8; 32],
        response: Vec<u8>,
    ) -> Result<()> {
        let mut connection = self.lock()?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        if let Some(prior) = entry(&tx, &self.namespace, operation_id)? {
            if prior.verb != verb
                || prior.request_hash != request_hash
                || (!prior.pending && prior.response != response)
            {
                return Err(HeddleError::Conflict(
                    "operation receipt does not match reserved or completed command".into(),
                ));
            }
            if !prior.pending {
                return Ok(());
            }
            tx.execute("UPDATE operation_receipts SET response=?2,pending=0,created_at=?3 WHERE namespace=?4 AND operation_id=?1 AND pending=1",params![operation_id.to_string(),response,now_secs(),self.namespace]).map_err(database_error)?;
        } else {
            tx.execute(
                "INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?6,?1,?7,?2,?3,?4,?5,0)",
                params![
                    operation_id.to_string(),
                    verb,
                    request_hash.as_slice(),
                    response,
                    now_secs(),
                    self.namespace,
                    self.record_key(operation_id)
                ],
            )
            .map_err(database_error)?;
        }
        prune(&tx, now_secs().saturating_sub(DEFAULT_RETENTION_SECS))?;
        tx.commit().map_err(database_error)?;
        drop(connection);
        self.notify_committed()?;
        Ok(())
    }
    pub fn cancel(&self, operation_id: OperationId, verb: &str) -> Result<()> {
        let changed = self.lock()?
            .execute(
                "DELETE FROM operation_receipts WHERE namespace=?3 AND operation_id=?1 AND verb=?2 AND pending=1",
                params![operation_id.to_string(), verb, self.namespace],
            )
            .map_err(database_error)?;
        if changed > 0 {
            self.notify_committed()?;
        }
        Ok(())
    }
    /// One bounded cleanup batch; pending commands never expire automatically.
    pub fn compact(&self, retention_secs: i64) -> Result<usize> {
        let connection = self.lock()?;
        let changed = prune(&connection, now_secs().saturating_sub(retention_secs))?;
        drop(connection);
        if changed > 0 {
            self.notify_committed()?;
        }
        Ok(changed)
    }
    pub fn len(&self) -> Result<usize> {
        let count: i64 = self
            .lock()?
            .query_row(
                "SELECT COUNT(*) FROM operation_receipts WHERE namespace=?1",
                [&self.namespace],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        usize::try_from(count).map_err(database_error)
    }
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
    pub fn metadata_for(
        &self,
        operation_id: OperationId,
        _verb: &str,
    ) -> Result<Option<DedupConflictMetadata>> {
        let connection = self.lock()?;
        connection.query_row("SELECT verb,request_hash,created_at,pending FROM operation_receipts WHERE namespace=?3 AND operation_id=?1 AND (pending=1 OR created_at>=?2)",params![operation_id.to_string(),now_secs().saturating_sub(DEFAULT_RETENTION_SECS),self.namespace],|row| Ok(DedupConflictMetadata {
            operation_id,verb:row.get(0)?,request_hash:row.get(1)?,created_at_secs:row.get(2)?,pending:row.get(3)?,
        })).optional().map_err(database_error)
    }
}
fn entry(
    connection: &Connection,
    namespace: &str,
    operation_id: OperationId,
) -> Result<Option<DedupEntry>> {
    connection.query_row("SELECT verb,request_hash,response,created_at,pending FROM operation_receipts WHERE namespace=?2 AND operation_id=?1",params![operation_id.to_string(),namespace],|row| Ok(DedupEntry {
        operation_id,verb:row.get(0)?,request_hash:row.get(1)?,response:row.get(2)?,created_at_secs:row.get(3)?,pending:row.get(4)?,
    })).optional().map_err(database_error)
}
fn prune(connection: &Connection, cutoff: i64) -> Result<usize> {
    connection.execute("DELETE FROM operation_receipts WHERE (namespace,operation_id) IN (SELECT namespace,operation_id FROM operation_receipts WHERE pending=0 AND created_at<?1 ORDER BY created_at,operation_id LIMIT ?2)",params![cutoff,COMPACTION_BATCH as i64]).map_err(database_error)
}

/// Record references remain globally unambiguous even when agents independently
/// choose the same client operation UUID. Possession of this ID is not authority.
pub fn receipt_record_key(namespace: &str, id: OperationId) -> objects::object::ContentHash {
    let mut hash = blake3::Hasher::new_derive_key("heddle-device-operation-record-v2");
    hash.update(&(namespace.len() as u64).to_be_bytes());
    hash.update(namespace.as_bytes());
    hash.update(id.as_bytes());
    objects::object::ContentHash::from_bytes(*hash.finalize().as_bytes())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compute the canonical request hash. Helper centralising the hashing
/// scheme so all callers (CLI verbs, hosted handlers) hash identically.
pub fn hash_request_body(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use objects::error::{HeddleError, Result};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        Repository,
        atomic::{AtomicMutation, StagedCommit, Tx, execute},
        operation_dedup::{ReserveOpId, reserve_operation_id_eager},
    };

    fn make_store() -> (TempDir, OperationDedupStore) {
        let temp = TempDir::new().unwrap();
        // Mimic the layout `Repository::open` would produce.
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let store = OperationDedupStore::open(&heddle).unwrap();
        (temp, store)
    }

    fn make_repo_store() -> (TempDir, Repository, Arc<OperationDedupStore>) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let store = Arc::new(OperationDedupStore::open(repo.heddle_dir()).unwrap());
        (temp, repo, store)
    }

    struct ReserveThenAbort {
        store: Arc<OperationDedupStore>,
        op: OperationId,
        hash: [u8; 32],
    }

    impl AtomicMutation for ReserveThenAbort {
        type Output = ();

        fn transaction_id(&self) -> String {
            format!("reserve-then-abort/{}", self.op)
        }

        fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
            Ok(BTreeSet::new())
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<()>> {
            tx.enroll_eager(ReserveOpId::new(
                Arc::clone(&self.store),
                self.op,
                "capture",
                self.hash,
            ))?;
            Err(HeddleError::Config("outer transaction aborted".to_string()))
        }
    }

    #[test]
    fn authenticated_namespaces_isolate_replay_pending_and_cleanup() {
        let (directory, cli) = make_store();
        let metadata = directory.path().join(".heddle");
        let first = OperationDedupStore::open_scoped(&metadata, "principal-a/agent-a")
            .expect("first namespace");
        let second = OperationDedupStore::open_scoped(&metadata, "principal-a/agent-b")
            .expect("second namespace");
        let id = OperationId::new();
        assert!(matches!(
            first
                .reserve(id, "capture", [1; 32])
                .expect("first reserve"),
            DedupOutcome::Reserved
        ));
        assert!(
            matches!(
                second
                    .reserve(id, "capture", [2; 32])
                    .expect("second reserve"),
                DedupOutcome::Reserved
            ),
            "same caller operation ID may exist in independent authenticated namespaces"
        );
        assert!(matches!(
            cli.reserve(id, "capture", [3; 32])
                .expect("local CLI reserve"),
            DedupOutcome::Reserved
        ));
        first
            .record(id, "capture", [1; 32], b"first response".to_vec())
            .expect("first response");
        assert!(
            matches!(
                second
                    .reserve(id, "capture", [2; 32])
                    .expect("second remains pending"),
                DedupOutcome::InFlight
            ),
            "another namespace must never receive completed response"
        );
        assert!(
            matches!(first.reserve(id,"capture",[1;32]).expect("first replay"), DedupOutcome::Replay {response} if response==b"first response")
        );
        first
            .lock()
            .expect("database")
            .execute(
                "UPDATE operation_receipts SET created_at=1 WHERE namespace=?1",
                ["principal-a/agent-a"],
            )
            .expect("age completed first command");
        first
            .compact(DEFAULT_RETENTION_SECS)
            .expect("bounded cleanup");
        assert_eq!(first.len().expect("first count"), 0);
        assert_eq!(
            second.len().expect("second count"),
            1,
            "cleanup cannot delete a pending command sharing another namespace's expired ID"
        );
        assert_eq!(cli.len().expect("CLI count"), 1);
        let visible = observation::page(&metadata, "principal-a/agent-b", &[id], &[], None, 10)
            .expect("scoped metadata");
        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0].record,
            receipt_record_key("principal-a/agent-b", id)
        );
        assert!(visible[0].pending);
        assert!(
            observation::page(
                &metadata,
                "principal-a/agent-a",
                &[],
                &[visible[0].record],
                None,
                10
            )
            .expect("foreign ref lookup")
            .is_empty(),
            "record references do not broaden caller namespace"
        );
        assert!(
            observation::page(&metadata, "", &[], &[], None, 10).is_err(),
            "device view cannot expose CLI namespace"
        );
        second
            .cancel(id, "capture")
            .expect("cancel second reservation");
        assert_eq!(second.len().expect("second cleared"), 0);
        assert_eq!(
            cli.len().expect("CLI untouched"),
            1,
            "cancel stays in exact authenticated namespace"
        );
    }

    #[test]
    fn receipt_notifications_follow_committed_mutations_only() {
        let (directory, _) = make_store();
        let metadata = directory.path().join(".heddle");
        let store =
            OperationDedupStore::open_scoped(&metadata, "owner/agent").expect("scoped store");
        let marker = metadata.join(crate::local_metadata::CHANGE_MARKER_NAME);
        let initial = observation::generation(&metadata).expect("initial cursor");
        for _ in 0..2 {
            let id = OperationId::new();
            store.reserve(id, "capture", [1; 32]).expect("reserve");
            let reserved = std::fs::read(&marker).expect("reservation committed wake");
            let cursor = observation::generation(&metadata).expect("reservation cursor");
            assert_ne!(
                cursor, initial,
                "reservation and durable cursor commit together"
            );
            assert_eq!(
                store.reserve(id, "capture", [1; 32]).expect("retry"),
                DedupOutcome::InFlight
            );
            assert_eq!(
                std::fs::read(&marker).expect("same marker"),
                reserved,
                "read-only replay cannot wake observers"
            );
            store
                .record(id, "capture", [1; 32], vec![1])
                .expect("complete");
            assert_ne!(
                std::fs::read(&marker).expect("completion wake"),
                reserved,
                "completion must notify committed observers"
            );
            assert_ne!(
                observation::generation(&metadata).expect("completion cursor"),
                cursor
            );
        }
    }

    #[test]
    fn eager_reserve_survives_outer_abort() {
        let (_t, repo, store) = make_repo_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        let result = execute(
            &repo,
            ReserveThenAbort {
                store: Arc::clone(&store),
                op,
                hash,
            },
        );

        assert!(result.is_err(), "outer transaction must abort");
        let reopened = OperationDedupStore::open(repo.heddle_dir()).unwrap();
        assert_eq!(
            reopened.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::InFlight,
            "eager op-id reservation must remain durable after outer abort"
        );
    }

    #[test]
    fn eager_parallel_distinct_reserves_both_win() {
        let (_t, repo, store) = make_repo_store();
        let op_a = OperationId::new();
        let op_b = OperationId::new();
        let hash = hash_request_body(b"x");

        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                reserve_operation_id_eager(&repo, Arc::clone(&store), op_a, "capture", hash)
                    .unwrap()
            });
            let b = scope.spawn(|| {
                reserve_operation_id_eager(&repo, Arc::clone(&store), op_b, "capture", hash)
                    .unwrap()
            });

            assert_eq!(a.join().unwrap(), DedupOutcome::Reserved);
            assert_eq!(b.join().unwrap(), DedupOutcome::Reserved);
        });

        assert_eq!(store.len().expect("count"), 2);
    }

    #[test]
    fn reserve_then_record_then_replay() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let body = b"hello";
        let hash = hash_request_body(body);

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        store
            .record(op, "capture", hash, b"response-1".to_vec())
            .unwrap();

        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"response-1"),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn second_reserve_with_same_body_sees_in_flight() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::InFlight
        );
    }

    #[test]
    fn cancel_releases_reservation() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        store.cancel(op, "capture").unwrap();
        // Slot is now free; a follow-up retry can re-claim it.
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
    }

    #[test]
    fn cancel_does_not_clobber_completed_record() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        // cancel must be a no-op against finalised entries — otherwise a
        // late-arriving cancel from a crashed retry could wipe the cached
        // response of a successful prior call.
        store.cancel(op, "capture").unwrap();
        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"r"),
            other => panic!("expected replay, got {other:?}"),
        }
    }

    #[test]
    fn conflict_on_different_body() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash_a = hash_request_body(b"a");
        let hash_b = hash_request_body(b"b");

        store
            .record(op, "capture", hash_a, b"resp".to_vec())
            .unwrap();
        assert_eq!(
            store.reserve(op, "capture", hash_b).unwrap(),
            DedupOutcome::Conflict
        );
    }

    #[test]
    fn same_op_id_with_different_verb_conflicts() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r1".to_vec()).unwrap();
        assert_eq!(
            store.reserve(op, "merge", hash).unwrap(),
            DedupOutcome::Conflict
        );
        let metadata = store
            .metadata_for(op, "merge")
            .expect("metadata read")
            .expect("cross-verb conflict should expose recorded metadata");
        assert_eq!(metadata.verb, "capture");
    }

    #[test]
    fn persists_across_reopen() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        {
            let store = OperationDedupStore::open(&heddle).unwrap();
            store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        }
        let store = OperationDedupStore::open(&heddle).unwrap();
        match store.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"r"),
            other => panic!("expected replay after reopen, got {other:?}"),
        }
    }

    #[test]
    fn compact_drops_old_entries() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        assert_eq!(store.len().expect("count"), 1);
        // Retain only entries newer than 0 seconds — everything older than
        // "now" is technically fair game. We pick a tiny retention to force
        // compaction while still inside the test.
        let pruned = store.compact(-1).unwrap();
        assert_eq!(pruned, 1);
        assert_eq!(store.len().expect("count"), 0);
    }

    #[test]
    fn fresh_after_compaction() {
        let (_t, store) = make_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");
        store.record(op, "capture", hash, b"r".to_vec()).unwrap();
        store.compact(-1).unwrap();
        assert_eq!(
            store.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
    }

    /// Independent connections must see each other's committed reservations.
    #[test]
    fn second_store_handle_sees_first_handles_reservation() {
        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        let store_a = OperationDedupStore::open(&heddle).unwrap();
        let store_b = OperationDedupStore::open(&heddle).unwrap();

        assert_eq!(
            store_a.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::Reserved
        );
        assert_eq!(
            store_b.reserve(op, "capture", hash).unwrap(),
            DedupOutcome::InFlight,
            "store B must observe store A's pending reservation across handles"
        );

        store_a
            .record(op, "capture", hash, b"resp".to_vec())
            .unwrap();
        match store_b.reserve(op, "capture", hash).unwrap() {
            DedupOutcome::Replay { response } => assert_eq!(response, b"resp"),
            other => panic!("expected replay after record, got {other:?}"),
        }
    }

    /// Race two `OperationDedupStore` handles with a thread barrier so they
    /// hit `reserve` as close to simultaneously as the OS allows. Exactly
    /// one must observe `Reserved`; the loser must observe `InFlight`
    /// (matching body) — never both `Reserved`, which would let two
    /// callers execute the same client_operation_id.
    #[test]
    fn parallel_reserves_across_handles_serialize() {
        use std::sync::{Arc, Barrier};

        let temp = TempDir::new().unwrap();
        let heddle = temp.path().join(".heddle");
        std::fs::create_dir_all(&heddle).unwrap();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let heddle = heddle.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let store = OperationDedupStore::open(&heddle).unwrap();
                barrier.wait();
                store.reserve(op, "capture", hash).unwrap()
            }));
        }
        let outcomes: Vec<DedupOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let reserved = outcomes
            .iter()
            .filter(|o| matches!(o, DedupOutcome::Reserved))
            .count();
        let in_flight = outcomes
            .iter()
            .filter(|o| matches!(o, DedupOutcome::InFlight))
            .count();
        assert_eq!(
            reserved, 1,
            "exactly one parallel reserve must win: {outcomes:?}"
        );
        assert_eq!(
            in_flight, 1,
            "the losing reserve must see InFlight: {outcomes:?}"
        );
    }

    #[test]
    fn sqlite_receipts_preserve_completion_and_bound_cleanup_without_expiring_pending() {
        let (_temp, store) = make_store();
        let pending = OperationId::new();
        let completed = OperationId::new();
        let hash = hash_request_body(b"body");
        store.reserve(pending, "capture", hash).expect("reserve");
        store
            .record(completed, "capture", hash, b"exact".to_vec())
            .expect("record");
        assert!(
            store
                .record(completed, "capture", hash, b"different".to_vec())
                .is_err()
        );
        assert!(
            store
                .record(completed, "other", hash, b"exact".to_vec())
                .is_err()
        );
        assert!(
            store
                .record(pending, "capture", hash_request_body(b"other"), Vec::new())
                .is_err()
        );
        store
            .lock()
            .expect("db")
            .execute("UPDATE operation_receipts SET created_at=1", [])
            .expect("age");
        store
            .record(completed, "capture", hash, b"exact".to_vec())
            .expect("exact retry");
        assert_eq!(
            entry(&store.lock().expect("database"), "", completed)
                .expect("stored receipt")
                .expect("receipt")
                .created_at_secs,
            1,
            "retry never extends original retention"
        );
        assert_eq!(store.compact(DEFAULT_RETENTION_SECS).expect("cleanup"), 1);
        assert_eq!(
            store
                .reserve(pending, "capture", hash)
                .expect("old pending"),
            DedupOutcome::InFlight
        );
        let mut connection = store.lock().expect("db");
        let tx = connection.transaction().expect("batch");
        for index in 0..COMPACTION_BATCH + 3 {
            tx.execute(
                "INSERT INTO operation_receipts(operation_id,verb,request_hash,response,created_at,pending) VALUES(?1,'capture',zeroblob(32),x'',1,0)",
                [format!("batch-{index}")],
            )
            .expect("old completion");
        }
        tx.commit().expect("batch commit");
        drop(connection);
        assert_eq!(
            store
                .compact(DEFAULT_RETENTION_SECS)
                .expect("bounded cleanup"),
            COMPACTION_BATCH
        );
        assert_eq!(store.len().expect("remaining"), 4);
        store
            .reserve(OperationId::new(), "capture", hash)
            .expect("automatic cleanup on reserve");
        assert_eq!(store.len().expect("remaining after automatic cleanup"), 2);
    }

    #[test]
    fn bootstrap_receipts_are_explicit_private_and_isolated_from_repository() {
        let directory = TempDir::new().expect("home");
        let bootstrap = directory.path().join("bootstrap/scope");
        let store =
            OperationDedupStore::open_bootstrap(&bootstrap).expect("bootstrap before repo exists");
        let operation = OperationId::new();
        let hash = hash_request_body(b"init");
        store
            .record(operation, "init", hash, b"created".to_vec())
            .expect("record");
        drop(store);
        let store = OperationDedupStore::open_bootstrap(&bootstrap).expect("reopen");
        assert_eq!(
            store.reserve(operation, "init", hash).expect("replay"),
            DedupOutcome::Replay {
                response: b"created".to_vec()
            }
        );
        let tables: i64 = store
            .lock()
            .expect("db")
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='threads'",
                [],
                |row| row.get(0),
            )
            .expect("bootstrap schema");
        assert_eq!(tables, 0, "bootstrap is not a pretend repository");
        assert!(
            !bootstrap
                .join(crate::local_metadata::DATABASE_NAME)
                .exists()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(bootstrap.join("operation-receipts.sqlite3"))
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
        let repository = directory.path().join("repo");
        std::fs::create_dir(&repository).expect("repository root");
        let repo = OperationDedupStore::open(&repository).expect("repo metadata");
        assert_eq!(
            repo.reserve(operation, "capture", hash)
                .expect("independent scope"),
            DedupOutcome::Reserved
        );
    }

    struct ReserveThenConflictOnce {
        store: Arc<OperationDedupStore>,
        op: OperationId,
        hash: [u8; 32],
        injected: bool,
    }

    impl AtomicMutation for ReserveThenConflictOnce {
        type Output = DedupOutcome;

        fn transaction_id(&self) -> String {
            format!("reserve-conflict-once/{}", self.op)
        }

        fn isolation_keys(&self, _repo: &Repository) -> Result<BTreeSet<IsolationKey>> {
            let mut keys = BTreeSet::new();
            keys.insert(IsolationKey::Thread("main".to_string()));
            Ok(keys)
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> Result<StagedCommit<DedupOutcome>> {
            let claim = tx.enroll_eager(ReserveOpId::new(
                Arc::clone(&self.store),
                self.op,
                "capture",
                self.hash,
            ))?;
            if !self.injected {
                self.injected = true;
                let state = crate::test_state_id();
                tx.repo().oplog().record_batch(vec![
                    oplog::OpRecord::Snapshot {
                        new_state: state,
                        prev_head: None,
                        head: None,
                        thread: Some("main".to_string()),
                    },
                    oplog::OpRecord::TransactionCommit {
                        transaction_id: "reserve-conflict-racer".to_string(),
                        op_count: 1,
                    },
                ])?;
            }
            Ok(StagedCommit::pure(claim.into_outcome()?))
        }
    }

    #[test]
    fn eager_reserve_observes_existing_reservation_after_conflict_retry() {
        let (_t, repo, store) = make_repo_store();
        let op = OperationId::new();
        let hash = hash_request_body(b"x");

        let outcome = execute(
            &repo,
            ReserveThenConflictOnce {
                store: Arc::clone(&store),
                op,
                hash,
                injected: false,
            },
        )
        .unwrap();

        assert_eq!(
            outcome,
            DedupOutcome::InFlight,
            "retrying the same op-id reserve must observe the first attempt's pending slot"
        );
        assert_eq!(
            store.len().expect("count"),
            1,
            "the retry must not create a second slot"
        );
    }
}
