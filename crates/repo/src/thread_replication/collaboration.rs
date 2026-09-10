//! Indexed collaboration heads and atomic command receipts over the same signed replica.
use std::collections::BTreeSet;

use crypto::thread_operation::SignedOperation;
use objects::{
    object::{
        CollaborationOperationEnvelope, ContentHash,
        thread_replication::{Admission, ThreadOperation, ThreadOperationBody},
    },
    store::ObjectStore,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Error, Result, ThreadReplica, hash};

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS collaboration_operations(thread BLOB NOT NULL,record_kind INTEGER NOT NULL,record_id TEXT NOT NULL,operation BLOB NOT NULL,is_append INTEGER NOT NULL,is_open INTEGER NOT NULL,collaboration_id BLOB,turn_count INTEGER NOT NULL,context_supersedes TEXT,PRIMARY KEY(thread,record_kind,record_id,operation)); CREATE INDEX IF NOT EXISTS collaboration_operation_id ON collaboration_operations(operation);";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub kind: i32,
    pub id: String,
}
#[derive(Clone, Debug)]
pub struct Heads {
    pub parents: BTreeSet<ContentHash>,
    pub version: Vec<u8>,
}
impl Heads {
    pub fn is_empty(&self) -> bool {
        self.parents.is_empty()
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub fn records(operation: &ThreadOperation) -> Result<Vec<Record>> {
    let mut records = Vec::new();
    if let ThreadOperationBody::Discussion(bytes) = &operation.body {
        let value = CollaborationOperationEnvelope::decode(bytes)
            .map_err(|error| invalid(error.to_string()))?;
        records.push(Record {
            kind: 1,
            id: value.operation.discussion_id.to_string(),
        });
    }
    if let Some(value) = operation.context_revision()? {
        records.push(Record {
            kind: 2,
            id: value.id.to_string(),
        });
    }
    Ok(records)
}
pub(super) fn index_operation(tx: &Transaction<'_>, operation: &ThreadOperation) -> Result<()> {
    super::collaboration_search::index(tx, operation)?;
    use objects::object::CollaborationOperationBodyV1 as Body;
    let (is_append, is_open, inner, turns) =
        if let ThreadOperationBody::Discussion(bytes) = &operation.body {
            let decoded = CollaborationOperationEnvelope::decode(bytes)
                .map_err(|error| invalid(error.to_string()))?;
            let turns = match &decoded.operation.body {
                Body::Open { .. } | Body::AppendTurn { .. } => 1,
                Body::LegacyImported { turns, .. } => turns.len(),
                _ => 0,
            };
            (
                matches!(decoded.operation.body, Body::AppendTurn { .. }),
                matches!(
                    decoded.operation.body,
                    Body::Open { .. } | Body::LegacyImported { .. }
                ),
                Some(decoded.operation_id.as_bytes().to_vec()),
                turns,
            )
        } else {
            (false, false, None, 0)
        };
    let supersedes = operation
        .context_revision()?
        .and_then(|record| record.supersedes)
        .map(|id| id.to_string());
    for record in records(operation)? {
        tx.execute("INSERT OR IGNORE INTO collaboration_operations(thread,record_kind,record_id,operation,is_append,is_open,collaboration_id,turn_count,context_supersedes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![operation.thread.as_bytes(),record.kind,record.id,operation.id()?.as_bytes(),is_append,is_open,inner,turns as i64,if record.kind==2{supersedes.clone()}else{None}])?;
    }
    Ok(())
}

fn heads(connection: &Connection, thread: ContentHash, record: &Record) -> Result<Heads> {
    let mut query=connection.prepare("SELECT x.operation FROM collaboration_operations x JOIN operations o ON o.id=x.operation WHERE x.thread=?1 AND x.record_kind=?2 AND x.record_id=?3 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child JOIN collaboration_operations cx ON cx.operation=c.id WHERE p.parent=o.id AND c.status=1 AND cx.thread=x.thread AND cx.record_kind=x.record_kind AND cx.record_id=x.record_id) ORDER BY x.operation LIMIT 129")?;
    let parents = query
        .query_map(params![thread.as_bytes(), record.kind, record.id], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .map(|row| hash(&row?))
        .collect::<Result<BTreeSet<_>>>()?;
    if parents.len() > 128 {
        return Err(invalid("collaboration frontier exceeds command budget"));
    }
    let mut digest = blake3::Hasher::new_derive_key("heddle-collaboration-view-v2");
    for id in &parents {
        digest.update(id.as_bytes());
    }
    Ok(Heads {
        parents,
        version: digest.finalize().as_bytes().to_vec(),
    })
}
pub enum Precondition {
    New,
    Exists,
    Exact(Vec<u8>),
    Append,
}
pub struct Command<'a> {
    pub namespace: String,
    pub id: uuid::Uuid,
    pub method: &'a str,
    pub request_hash: [u8; 32],
    pub record: Record,
    pub precondition: Precondition,
}
impl ThreadReplica {
    pub fn collaboration_heads(&self, record: &Record) -> Result<Heads> {
        heads(&self.connect()?, self.thread, record)
    }
    /// Validation, observed-head CAS, causal admission and exact response share one commit.
    pub fn collaboration_command(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        command: Command<'_>,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
        response: impl FnOnce(&[(Record, Heads)]) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        if command.namespace.is_empty() || command.namespace.len() > 1024 {
            return Err(invalid("authenticated command namespace required"));
        }
        let operation = signed.verify()?;
        if operation.thread != self.thread
            || !matches!(
                operation.body,
                ThreadOperationBody::Discussion(_) | ThreadOperationBody::Context(_)
            )
        {
            return Err(invalid(
                "collaboration command requires exact Thread discussion/context",
            ));
        }
        authorize(&operation)?;
        let records = records(&operation)?;
        if records.first() != Some(&command.record) {
            return Err(invalid("collaboration record differs from signed scope"));
        }
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prior:Option<(String,Vec<u8>,Vec<u8>,bool)>=tx.query_row("SELECT verb,request_hash,response,pending FROM operation_receipts WHERE namespace=?2 AND operation_id=?1",params![command.id.to_string(),command.namespace],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        if let Some((method, digest, response, pending)) = prior {
            if method != command.method || digest != command.request_hash {
                return Err(invalid("operation ID names a different command"));
            }
            if pending {
                return Err(invalid("operation already in flight"));
            }
            return Ok(response);
        }
        let before = heads(&tx, self.thread, &command.record)?;
        match &command.precondition {
            Precondition::New if !before.is_empty() || !operation.parents.is_empty() => {
                return Err(invalid("collaboration record already exists"));
            }
            Precondition::Exists | Precondition::Append if before.is_empty() => {
                return Err(invalid("collaboration record unavailable"));
            }
            Precondition::Exact(expected)
                if expected.is_empty()
                    || *expected != before.version
                    || operation.parents != before.parents =>
            {
                return Err(invalid(
                    "collaboration heads changed; refresh before mutation",
                ));
            }
            _ => {}
        }
        for extra in records.iter().skip(1) {
            if !heads(&tx, self.thread, extra)?.is_empty() {
                return Err(invalid("extracted context already exists"));
            }
        }
        let admission = self.receive_in(&tx, signed, &operation, store, false, None)?;
        if admission != Admission::Accepted {
            return Err(invalid("collaboration requires admitted causal parents"));
        }
        let after = records
            .into_iter()
            .map(|record| Ok((record.clone(), heads(&tx, self.thread, &record)?)))
            .collect::<Result<Vec<_>>>()?;
        let response = response(&after)?;
        tx.execute("INSERT INTO operation_receipts(namespace,operation_id,record_id,verb,request_hash,response,created_at,pending) VALUES(?6,?1,?7,?2,?3,?4,?5,0)",params![command.id.to_string(),command.method,command.request_hash.as_slice(),response,chrono::Utc::now().timestamp(),command.namespace,crate::operation_dedup::receipt_record_key(&command.namespace,objects::object::OperationId::from_uuid(command.id)).as_bytes().as_slice()])?;
        tx.commit()?;
        self.notify_committed()?;
        Ok(response)
    }
}

#[derive(Clone, Debug)]
pub struct Position {
    pub kind: i32,
    pub thread: ContentHash,
    pub operation: ContentHash,
}
pub struct Candidate {
    pub position: Position,
    pub signed: SignedOperation,
}
/// Candidate rows are independently paged; turn history never needs materialization.
pub fn candidate_page(
    directory: &std::path::Path,
    thread: Option<ContentHash>,
    after: Option<&Position>,
    history: bool,
    proofs: bool,
    limit: usize,
    max_bytes: usize,
) -> Result<Vec<Candidate>> {
    if !(1..=1024).contains(&limit) {
        return Err(invalid("collaboration candidate page must be 1..1024"));
    }
    let connection = crate::local_metadata::open(directory)?;
    let thread_filter = if thread.is_some() {
        " AND x.thread=?7"
    } else {
        " AND ?7 IS NULL"
    };
    let sql = format!(
        "WITH kinds(kind) AS (VALUES(0),(1),(2),(3)) SELECT k.kind,o.thread,o.id,o.canonical,o.signature FROM operations o JOIN collaboration_operations x ON x.operation=o.id CROSS JOIN kinds k WHERE o.status=1{thread_filter} AND ((k.kind=0 AND x.record_kind=1 AND x.is_open=1) OR (k.kind=1 AND x.record_kind=1 AND x.turn_count>0 AND ?1) OR (k.kind=2 AND x.record_kind=2 AND (?1 OR NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child JOIN collaboration_operations cx ON cx.operation=c.id WHERE p.parent=o.id AND c.status=1 AND cx.thread=x.thread AND cx.record_kind=x.record_kind AND cx.record_id=x.record_id))) OR (k.kind=3 AND ?2 AND x.record_kind=(SELECT MIN(record_kind) FROM collaboration_operations xx WHERE xx.operation=o.id))) AND (?3 IS NULL OR (k.kind,o.thread,o.id)>(?3,?4,?5)) ORDER BY k.kind,o.thread,o.id LIMIT ?6"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut bytes = 0usize;
    statement
        .query_map(
            params![
                history,
                proofs,
                after.map(|value| value.kind),
                after.map(|value| value.thread.as_bytes().to_vec()),
                after.map(|value| value.operation.as_bytes().to_vec()),
                limit as i64,
                thread.map(|thread| thread.as_bytes().to_vec())
            ],
            |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )?
        .map(|row| {
            let (kind, thread, id, canonical, signature) = row?;
            bytes = bytes
                .saturating_add(canonical.len())
                .saturating_add(signature.len());
            if bytes > max_bytes {
                return Err(invalid("collaboration page exceeds decode budget"));
            }
            Ok(Candidate {
                position: Position {
                    kind,
                    thread: hash(&thread)?,
                    operation: hash(&id)?,
                },
                signed: SignedOperation {
                    canonical,
                    signature,
                },
            })
        })
        .collect()
}
pub struct DiscussionSummary {
    pub discussion: objects::object::MaterializedDiscussion,
    pub heads: Heads,
    pub turn_count: u64,
}
impl ThreadReplica {
    pub fn discussion_summary(
        &self,
        discussion: objects::object::DiscussionRecordId,
        max_bytes: usize,
    ) -> Result<DiscussionSummary> {
        let mut connection = self.connect()?;
        let tx = connection.transaction()?;
        let record = Record {
            kind: 1,
            id: discussion.to_string(),
        };
        let mut query=tx.prepare("SELECT o.id,o.canonical FROM collaboration_operations x JOIN operations o ON o.id=x.operation WHERE x.thread=?1 AND x.record_kind=1 AND x.record_id=?2 AND o.status=1 AND x.is_append=0 ORDER BY o.id LIMIT 129")?;
        let values = query
            .query_map(params![self.thread.as_bytes(), record.id], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if values.len() > 128 {
            return Err(invalid("discussion state graph exceeds view budget"));
        }
        let mut used = 0;
        let mut records = Vec::new();
        for (id, canonical) in values {
            used += canonical.len();
            if used > max_bytes {
                return Err(invalid("discussion state graph exceeds byte budget"));
            }
            let operation = ThreadOperation::decode(&canonical)?;
            let ThreadOperationBody::Discussion(bytes) = operation.body else {
                return Err(invalid("discussion index facet mismatch"));
            };
            let mut decoded = CollaborationOperationEnvelope::decode(&bytes)
                .map_err(|error| invalid(error.to_string()))?;
            let mut parents=tx.prepare("WITH RECURSIVE contracted(parent) AS (SELECT parent FROM parents WHERE child=?1 UNION SELECT p.parent FROM contracted c JOIN collaboration_operations a ON a.operation=c.parent AND a.record_kind=1 AND a.is_append=1 JOIN parents p ON p.child=a.operation) SELECT DISTINCT a.collaboration_id FROM contracted c JOIN collaboration_operations a ON a.operation=c.parent AND a.record_kind=1 AND a.is_append=0 ORDER BY a.collaboration_id LIMIT 129")?;
            let parents = parents
                .query_map([id], |row| row.get::<_, Vec<u8>>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if parents.len() > 128 {
                return Err(invalid("discussion contracted frontier exceeds budget"));
            }
            decoded.operation.parents = parents
                .into_iter()
                .map(|id| {
                    Ok(objects::object::CollabOpId::from_bytes(
                        id.as_slice()
                            .try_into()
                            .map_err(|_| invalid("invalid collaboration causal identity"))?,
                    ))
                })
                .collect::<Result<_>>()?;
            records.push(decoded);
        }
        drop(query);
        let mut materialized = objects::object::materialize_repository_collaboration(records)
            .map_err(|error| invalid(error.to_string()))?;
        if !materialized.pending.is_empty() {
            return Err(invalid(
                "accepted discussion has unresolved state ancestors",
            ));
        }
        let projected = materialized
            .discussions
            .remove(&discussion)
            .ok_or_else(|| invalid("discussion unavailable"))?;
        let heads = heads(&tx, self.thread, &record)?;
        let turn_count:i64=tx.query_row("SELECT COALESCE(SUM(x.turn_count),0) FROM collaboration_operations x JOIN operations o ON o.id=x.operation WHERE x.thread=?1 AND x.record_kind=1 AND x.record_id=?2 AND o.status=1",params![self.thread.as_bytes(),record.id],|row|row.get(0))?;
        tx.commit()?;
        Ok(DiscussionSummary {
            discussion: projected,
            heads,
            turn_count: u64::try_from(turn_count).map_err(|_| invalid("negative turn count"))?,
        })
    }
}
/// Scalar commit generation for the shared Spool view; queried on wake, never a timer.
pub fn generation(directory: &std::path::Path) -> Result<Vec<u8>> {
    let connection = crate::local_metadata::open(directory)?;
    let (count, total): (i64, i64) = connection.query_row(
        "SELECT COUNT(*),COALESCE(SUM(generation),0) FROM threads",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok([count.to_be_bytes(), total.to_be_bytes()].concat())
}

impl ThreadReplica {
    pub fn context_superseded(&self, id: uuid::Uuid) -> Result<bool> {
        Ok(self.connect()?.query_row("SELECT EXISTS(SELECT 1 FROM collaboration_operations x JOIN operations o ON o.id=x.operation WHERE x.thread=?1 AND x.record_kind=2 AND x.context_supersedes=?2 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child JOIN collaboration_operations cx ON cx.operation=c.id WHERE p.parent=o.id AND c.status=1 AND cx.thread=x.thread AND cx.record_kind=x.record_kind AND cx.record_id=x.record_id))",params![self.thread.as_bytes(),id.to_string()],|row|row.get(0))?)
    }
}
