// SPDX-License-Identifier: Apache-2.0
//! Durable Thread replication over native captures and collaboration operations.
//! Endpoint adapters must authorize the exact Thread and disclosure facets before
//! calling receive/export. Signatures prove the publisher, not spool membership.
pub mod checkout;
mod peers;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use crypto::thread_operation::SignedOperation;
use objects::{
    object::{
        CollaborationOperationEnvelope, ContentHash, MaterializedRepositoryCollaboration, State,
        StateId, materialize_repository_collaboration,
        thread_replication::{
            Admission, ThreadFacet, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    store::ObjectStore,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Thread storage: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("Thread object: {0}")]
    Object(#[from] objects::error::HeddleError),
    #[error("Thread filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("Thread signature: {0}")]
    Signature(#[from] crypto::SignerError),
    #[error(transparent)]
    SignedOperation(#[from] crypto::thread_operation::Error),
    #[error("{0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
pub struct ThreadView {
    pub generation: i64,
    pub frontiers: BTreeMap<ThreadFacet, BTreeSet<ContentHash>>,
    pub source_heads: BTreeSet<StateId>,
    pub collaboration: MaterializedRepositoryCollaboration,
    pub pending: BTreeSet<ContentHash>,
    pub rejected: BTreeMap<ContentHash, String>,
}

/// Cheap to clone across stream workers. Connections and transactions remain
/// local to each operation; SQLite serializes commits across processes.
#[derive(Clone)]
pub struct ThreadReplica {
    path: PathBuf,
    thread: ContentHash,
}
impl ThreadReplica {
    pub fn open(heddle_dir: &Path, genesis: &ThreadGenesis) -> Result<Self> {
        objects::fs_atomic::create_dir_all_durable(heddle_dir)?;
        let this = Self {
            path: heddle_dir.join("thread-replication.sqlite3"),
            thread: genesis.id()?,
        };
        let connection = this.connect()?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS threads(id BLOB PRIMARY KEY, genesis BLOB NOT NULL, generation INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS operations(id BLOB PRIMARY KEY, thread BLOB NOT NULL, facet INTEGER NOT NULL, canonical BLOB NOT NULL, signature BLOB NOT NULL, status INTEGER NOT NULL DEFAULT 0, reason TEXT);
            CREATE INDEX IF NOT EXISTS operations_thread_status ON operations(thread,status);
            CREATE TABLE IF NOT EXISTS parents(child BLOB NOT NULL, parent BLOB NOT NULL, PRIMARY KEY(child,parent));
            CREATE INDEX IF NOT EXISTS parents_parent ON parents(parent);
            CREATE TABLE IF NOT EXISTS request_nonces(identity TEXT NOT NULL, nonce BLOB NOT NULL, expires INTEGER NOT NULL, PRIMARY KEY(identity,nonce));
            CREATE TABLE IF NOT EXISTS peer_heads(thread BLOB NOT NULL, peer BLOB NOT NULL, operation BLOB NOT NULL, facet INTEGER NOT NULL, PRIMARY KEY(thread,peer,operation));
            CREATE TABLE IF NOT EXISTS peer_receipts(thread BLOB NOT NULL, peer BLOB NOT NULL, operation BLOB NOT NULL, status INTEGER NOT NULL, reason TEXT, PRIMARY KEY(thread,peer,operation));
            CREATE TABLE IF NOT EXISTS sharing(thread BLOB NOT NULL, destination BLOB NOT NULL, source INTEGER NOT NULL, discussion INTEGER NOT NULL, version BLOB NOT NULL, PRIMARY KEY(thread,destination));")?;
        connection.execute(
            "INSERT OR IGNORE INTO threads(id,genesis) VALUES(?1,?2)",
            params![this.thread.as_bytes(), genesis.encode()?],
        )?;
        let stored: Vec<u8> = connection.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [this.thread.as_bytes()],
            |r| r.get(0),
        )?;
        if stored != genesis.encode()? {
            return Err(Error::Invalid("Thread genesis collision".into()));
        }
        Ok(this)
    }
    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        Ok(connection)
    }
    pub fn thread_id(&self) -> ContentHash {
        self.thread
    }
    pub fn genesis(&self) -> Result<ThreadGenesis> {
        let connection = self.connect()?;
        let bytes: Vec<u8> = connection.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        Ok(ThreadGenesis::decode(&bytes)?)
    }
    pub fn generation(&self) -> Result<i64> {
        Ok(self.connect()?.query_row(
            "SELECT generation FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?)
    }

    /// Persist a verified operation and admit newly complete causal descendants.
    /// The caller's authorization is evaluated before any private bytes persist.
    /// Native capture objects become durable before the acceptance transaction.
    pub fn receive(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        let operation = signed.verify()?;
        if operation.thread != self.thread {
            return Err(Error::Invalid("wrong Thread".into()));
        }
        authorize(&operation)?;
        let id = operation.id()?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let inserted = tx.execute("INSERT OR IGNORE INTO operations(id,thread,facet,canonical,signature) VALUES(?1,?2,?3,?4,?5)",
            params![id.as_bytes(), self.thread.as_bytes(), facet_number(operation.facet()), signed.canonical, signed.signature])?;
        if inserted > 0 {
            for parent in &operation.parents {
                tx.execute(
                    "INSERT INTO parents(child,parent) VALUES(?1,?2)",
                    params![id.as_bytes(), parent.as_bytes()],
                )?;
            }
            tx.execute(
                "UPDATE threads SET generation=generation+1 WHERE id=?1",
                [self.thread.as_bytes()],
            )?;
        }
        self.admit_ready(&tx, store)?;
        let admission = status(&tx, &id)?;
        tx.commit()?;
        Ok(admission)
    }

    fn admit_ready(&self, tx: &Transaction<'_>, store: &impl ObjectStore) -> Result<()> {
        let genesis_bytes: Vec<u8> = tx.query_row(
            "SELECT genesis FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        let genesis = ThreadGenesis::decode(&genesis_bytes)?;
        loop {
            let ready: Vec<Vec<u8>> = tx.prepare("SELECT o.canonical FROM operations o WHERE o.thread=?1 AND o.status=0 AND (EXISTS (SELECT 1 FROM parents p JOIN operations a ON a.id=p.parent WHERE p.child=o.id AND (a.status=2 OR a.thread<>o.thread OR a.facet<>o.facet)) OR NOT EXISTS (SELECT 1 FROM parents p LEFT JOIN operations a ON a.id=p.parent WHERE p.child=o.id AND (a.id IS NULL OR a.status<>1))) ORDER BY o.id LIMIT 128")?
                .query_map([self.thread.as_bytes()], |r| r.get(0))?.collect::<std::result::Result<_,_>>()?;
            if ready.is_empty() {
                break;
            }
            for bytes in ready {
                let operation = ThreadOperation::decode(&bytes)?;
                let id = operation.id()?;
                let parent_failure: Option<String> = tx.query_row(
                    "SELECT CASE WHEN a.thread<>?2 OR a.facet<>?3 THEN 'causal parents cross Thread or disclosure facet' ELSE 'causal parent was rejected' END FROM parents p JOIN operations a ON a.id=p.parent WHERE p.child=?1 AND (a.status=2 OR a.thread<>?2 OR a.facet<>?3) ORDER BY a.id LIMIT 1",
                    params![id.as_bytes(), self.thread.as_bytes(), facet_number(operation.facet())],
                    |r| r.get(0),
                ).optional()?;
                if let Some(reason) = parent_failure {
                    tx.execute(
                        "UPDATE operations SET status=2,reason=?2 WHERE id=?1",
                        params![id.as_bytes(), reason],
                    )?;
                    tx.execute(
                        "UPDATE threads SET generation=generation+1 WHERE id=?1",
                        [self.thread.as_bytes()],
                    )?;
                    continue;
                }
                let mut parents = Vec::new();
                for parent in &operation.parents {
                    let bytes: Vec<u8> = tx.query_row(
                        "SELECT canonical FROM operations WHERE id=?1",
                        [parent.as_bytes()],
                        |r| r.get(0),
                    )?;
                    parents.push(ThreadOperation::decode(&bytes)?);
                }
                match operation.validate_parents(&genesis, &parents) {
                    Ok(()) => {
                        if let ThreadOperationBody::Capture(bytes) = &operation.body {
                            store.put_state(&State::decode_current_msgpack(bytes)?)?;
                        }
                        tx.execute(
                            "UPDATE operations SET status=1 WHERE id=?1",
                            [id.as_bytes()],
                        )?;
                    }
                    Err(error) => {
                        tx.execute(
                            "UPDATE operations SET status=2,reason=?2 WHERE id=?1",
                            params![id.as_bytes(), error.to_string()],
                        )?;
                    }
                }
                tx.execute(
                    "UPDATE threads SET generation=generation+1 WHERE id=?1",
                    [self.thread.as_bytes()],
                )?;
            }
        }
        Ok(())
    }

    /// Claim a verified request nonce durably, across streams and restarts.
    pub fn claim_request_nonce(&self, identity: &str, nonce: &[u8], now_ms: i64) -> Result<bool> {
        if nonce.len() != 16 {
            return Err(Error::Invalid("invalid request nonce".into()));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM request_nonces WHERE expires<=?1", [now_ms])?;
        let count: i64 = tx.query_row("SELECT count(*) FROM request_nonces", [], |r| r.get(0))?;
        if count >= 16384 {
            return Err(Error::Invalid("request replay registry at capacity".into()));
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO request_nonces(identity,nonce,expires) VALUES(?1,?2,?3)",
            params![identity, nonce, now_ms.saturating_add(120000)],
        )?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Find a bounded page of unknown ancestors below a persisted pending
    /// operation. Known pending parents are traversed after reconnect; accepted
    /// history is already closed and never needs to be walked again.
    pub fn missing_ancestors(&self, root: ContentHash, limit: usize) -> Result<Vec<ContentHash>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare(
            "WITH RECURSIVE unresolved(id) AS (
                SELECT id FROM operations WHERE id=?1 AND thread=?2 AND status=0
                UNION
                SELECT p.parent FROM unresolved u JOIN operations o ON o.id=u.id AND o.status=0 AND o.thread=?2 JOIN parents p ON p.child=o.id
            ) SELECT u.id FROM unresolved u LEFT JOIN operations o ON o.id=u.id WHERE o.id IS NULL ORDER BY u.id LIMIT ?3"
        )?;
        query
            .query_map(
                params![root.as_bytes(), self.thread.as_bytes(), limit as u32],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .map(|row| hash(&row?))
            .collect()
    }

    pub fn frontier_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<ContentHash>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=?2 AND o.status=1 AND (?3 IS NULL OR o.id>?3) AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT ?4")?;
        query
            .query_map(
                params![
                    self.thread.as_bytes(),
                    facet_number(facet),
                    after.map(|id| id.as_bytes().to_vec()),
                    limit as u32
                ],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .map(|row| hash(&row?))
            .collect()
    }

    pub fn operation(&self, id: &ContentHash) -> Result<Option<(SignedOperation, Admission)>> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT canonical,signature FROM operations WHERE id=?1 AND thread=?2",
                params![id.as_bytes(), self.thread.as_bytes()],
                |r| {
                    Ok(SignedOperation {
                        canonical: r.get(0)?,
                        signature: r.get(1)?,
                    })
                },
            )
            .optional()?;
        row.map(|record| Ok((record, status(&connection, id)?)))
            .transpose()
    }

    /// Bounded durable paging, also used after reconnect. The cursor is an
    /// operation ID within this Thread, not an observation cursor.
    pub fn accepted_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, SignedOperation)>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid("page size must be 1..1024".into()));
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT id,canonical,signature FROM operations WHERE thread=?1 AND status=1 AND facet=?2 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4")?;
        let rows = query.query_map(
            params![
                self.thread.as_bytes(),
                facet_number(facet),
                after.map(|id| id.as_bytes().to_vec()),
                limit as u32
            ],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    SignedOperation {
                        canonical: r.get(1)?,
                        signature: r.get(2)?,
                    },
                ))
            },
        )?;
        rows.map(|row| {
            let (id, record) = row?;
            Ok((hash(&id)?, record))
        })
        .collect()
    }

    /// Snapshot and generation are read in one SQLite transaction. Stream
    /// adapters compare the generation after subscribing, closing the handoff gap.
    pub fn view(&self) -> Result<ThreadView> {
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let generation = tx.query_row(
            "SELECT generation FROM threads WHERE id=?1",
            [self.thread.as_bytes()],
            |r| r.get(0),
        )?;
        let mut accepted = BTreeMap::new();
        let mut pending = BTreeSet::new();
        let mut rejected = BTreeMap::new();
        let rows = tx
            .prepare(
                "SELECT id,canonical,status,reason FROM operations WHERE thread=?1 ORDER BY id",
            )?
            .query_map([self.thread.as_bytes()], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, bytes, status, reason) in rows {
            let id = hash(&id)?;
            match status {
                1 => {
                    accepted.insert(id, ThreadOperation::decode(&bytes)?);
                }
                2 => {
                    rejected.insert(id, reason.unwrap_or_default());
                }
                _ => {
                    pending.insert(id);
                }
            }
        }
        let mut frontiers: BTreeMap<ThreadFacet, BTreeSet<ContentHash>> = BTreeMap::new();
        let mut discussion = BTreeMap::new();
        for (id, operation) in &accepted {
            frontiers.entry(operation.facet()).or_default().insert(*id);
            if let ThreadOperationBody::Discussion(bytes) = &operation.body {
                let decoded = CollaborationOperationEnvelope::decode(bytes)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
                discussion.insert(decoded.operation_id, decoded);
            }
        }
        for operation in accepted.values() {
            if let Some(heads) = frontiers.get_mut(&operation.facet()) {
                for parent in &operation.parents {
                    heads.remove(parent);
                }
            }
        }
        let mut source_heads = BTreeSet::new();
        if let Some(heads) = frontiers.get(&ThreadFacet::Source) {
            for head in heads {
                if let Some(ThreadOperation {
                    body: ThreadOperationBody::Capture(bytes),
                    ..
                }) = accepted.get(head)
                {
                    source_heads.insert(State::decode_current_msgpack(bytes)?.id());
                }
            }
        }
        let collaboration = materialize_repository_collaboration(discussion.into_values())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        tx.commit()?;
        Ok(ThreadView {
            generation,
            frontiers,
            source_heads,
            collaboration,
            pending,
            rejected,
        })
    }

    /// The caller must hold Thread policy-write authority. Empty facets revoke
    /// future export; revocation never claims to erase already disclosed bytes.
    pub fn set_sharing(
        &self,
        destination: [u8; 32],
        facets: &BTreeSet<ThreadFacet>,
    ) -> Result<ContentHash> {
        let source = facets.contains(&ThreadFacet::Source);
        let discussion = facets.contains(&ThreadFacet::Discussion);
        let mut bytes = self.thread.as_bytes().to_vec();
        bytes.extend(destination);
        bytes.push(u8::from(source));
        bytes.push(u8::from(discussion));
        let version = ContentHash::compute_typed("thread-sharing-v1", &bytes);
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO sharing(thread,destination,source,discussion,version) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(thread,destination) DO UPDATE SET source=excluded.source,discussion=excluded.discussion,version=excluded.version",params![self.thread.as_bytes(),destination,source,discussion,version.as_bytes()])?;
        tx.execute(
            "UPDATE threads SET generation=generation+1 WHERE id=?1",
            [self.thread.as_bytes()],
        )?;
        tx.commit()?;
        Ok(version)
    }

    pub fn sharing(
        &self,
        destination: &[u8; 32],
    ) -> Result<(BTreeSet<ThreadFacet>, Option<ContentHash>)> {
        let row: Option<(bool, bool, Vec<u8>)> = self
            .connect()?
            .query_row(
                "SELECT source,discussion,version FROM sharing WHERE thread=?1 AND destination=?2",
                params![self.thread.as_bytes(), destination],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((source, discussion, version)) = row else {
            return Ok((BTreeSet::new(), None));
        };
        let mut facets = BTreeSet::new();
        if source {
            facets.insert(ThreadFacet::Source);
        }
        if discussion {
            facets.insert(ThreadFacet::Discussion);
        }
        Ok((facets, Some(hash(&version)?)))
    }
}

fn status(connection: &Connection, id: &ContentHash) -> Result<Admission> {
    let (status, reason): (i32, Option<String>) = connection.query_row(
        "SELECT status,reason FROM operations WHERE id=?1",
        [id.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(match status {
        1 => Admission::Accepted,
        2 => Admission::Rejected(reason.unwrap_or_default()),
        _ => Admission::Pending,
    })
}
fn facet_number(facet: ThreadFacet) -> u32 {
    match facet {
        ThreadFacet::Source => 1,
        ThreadFacet::Discussion => 2,
    }
}
fn hash(bytes: &[u8]) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(bytes.try_into().map_err(|_| {
        Error::Invalid("invalid operation ID length".into())
    })?))
}

#[cfg(test)]
mod tests;
