//! Admission-maintained list summaries. Paging never parses historical operations.
use std::path::Path;

use objects::object::{
    ContentHash,
    thread_replication::{
        ThreadGenesis, ThreadOperationBody,
        metadata::{Control, Lifecycle, Property, ThreadControl},
    },
};
use rusqlite::{Transaction, params};

use super::{Error, Result, hash};

pub(super) const SCHEMA:&str="
CREATE TABLE IF NOT EXISTS thread_list_epoch(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
INSERT OR IGNORE INTO thread_list_epoch VALUES(1,0);
CREATE TRIGGER IF NOT EXISTS thread_list_epoch_insert AFTER INSERT ON threads BEGIN UPDATE thread_list_epoch SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS thread_list_epoch_update AFTER UPDATE OF generation ON threads BEGIN UPDATE thread_list_epoch SET version=version+1 WHERE id=1; END;
CREATE TABLE IF NOT EXISTS thread_list(thread BLOB PRIMARY KEY,name TEXT NOT NULL,intent TEXT NOT NULL,lifecycle INTEGER NOT NULL,parent BLOB,updated INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS thread_list_name ON thread_list(name,thread);
CREATE INDEX IF NOT EXISTS thread_list_updated ON thread_list(updated DESC,thread DESC);
CREATE TRIGGER IF NOT EXISTS thread_list_changed AFTER UPDATE OF generation ON threads BEGIN UPDATE thread_list SET updated=CAST((julianday('now')-2440587.5)*86400000 AS INTEGER) WHERE thread=NEW.id; END;";

pub(super) fn initialize(tx: &Transaction<'_>, genesis: &ThreadGenesis) -> Result<()> {
    tx.execute("INSERT OR IGNORE INTO thread_list(thread,name,intent,lifecycle,parent,updated) VALUES(?1,?2,?3,1,?4,CAST((julianday('now')-2440587.5)*86400000 AS INTEGER))",params![genesis.id()?.as_bytes(),genesis.name,genesis.intent,genesis.parent.map(|p|p.as_bytes().to_vec())])?;
    Ok(())
}
pub(super) fn refresh(
    tx: &Transaction<'_>,
    thread: ContentHash,
    property: &Property,
) -> Result<()> {
    if !matches!(
        property,
        Property::Name | Property::Intent | Property::Lifecycle
    ) {
        return Ok(());
    }
    let mut statement=tx.prepare("SELECT o.canonical FROM thread_control_heads h JOIN operations o ON o.id=h.operation AND o.thread=h.thread WHERE h.thread=?1 AND h.property=?2 ORDER BY h.operation LIMIT 2")?;
    let heads = statement
        .query_map(
            params![thread.as_bytes(), super::metadata::key(property)],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let canonical: Vec<u8> = tx.query_row(
        "SELECT genesis FROM threads WHERE id=?1",
        [thread.as_bytes()],
        |row| row.get(0),
    )?;
    let genesis = ThreadGenesis::decode(&canonical)?;
    // Multiple current candidates remain an explicit conflict in the exact view.
    // The list falls back to the immutable genesis/default, never arrival order.
    let control = if heads.len() == 1 {
        let op = objects::object::thread_replication::ThreadOperation::decode(&heads[0])?;
        let ThreadOperationBody::Metadata(bytes) = op.body else {
            return Err(Error::Invalid("list index names another facet".into()));
        };
        Some(ThreadControl::decode(&bytes)?.control)
    } else {
        None
    };
    match property {
        Property::Name => {
            let name = match control {
                Some(Control::Name(value)) => value,
                _ => genesis.name,
            };
            tx.execute(
                "UPDATE thread_list SET name=?2 WHERE thread=?1",
                params![thread.as_bytes(), name],
            )?;
        }
        Property::Intent => {
            let intent = match control {
                Some(Control::Intent(value)) => value.outcome,
                _ => genesis.intent,
            };
            tx.execute(
                "UPDATE thread_list SET intent=?2 WHERE thread=?1",
                params![thread.as_bytes(), intent],
            )?;
        }
        Property::Lifecycle => {
            let lifecycle = match control {
                Some(Control::Lifecycle(Lifecycle::Active)) => 2,
                Some(Control::Lifecycle(Lifecycle::Ready)) => 3,
                Some(Control::Lifecycle(Lifecycle::Abandoned)) => 5,
                _ => 1,
            };
            tx.execute(
                "UPDATE thread_list SET lifecycle=?2 WHERE thread=?1",
                params![thread.as_bytes(), lifecycle],
            )?;
        }
        _ => {}
    };
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Row {
    pub thread: ContentHash,
    pub name: String,
    pub intent: String,
    pub lifecycle: i32,
    pub parent: Option<ContentHash>,
    pub updated: i64,
}
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    pub name: String,
    pub updated: i64,
    pub thread: Vec<u8>,
}
/// At most one bounded candidate window. Callers apply filters to these summaries
/// and preserve the scanned cursor, including when no candidate matches.
pub fn page(
    heddle_dir: &Path,
    by_name: bool,
    after: Option<&Cursor>,
    limit: usize,
) -> Result<Vec<Row>> {
    if limit == 0 || limit > 1024 {
        return Err(Error::Invalid("Thread list limit must be 1..1024".into()));
    }
    let path = heddle_dir.join("thread-replication.sqlite3");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let default = Cursor {
        name: String::new(),
        updated: i64::MAX,
        thread: if by_name { Vec::new() } else { vec![255; 32] },
    };
    let after = after.unwrap_or(&default);
    let sql = if by_name {
        "SELECT thread,name,intent,lifecycle,parent,updated FROM thread_list WHERE (name,thread)>(?1,?2) ORDER BY name,thread LIMIT ?3"
    } else {
        "SELECT thread,name,intent,lifecycle,parent,updated FROM thread_list WHERE (updated,thread)<(?1,?2) ORDER BY updated DESC,thread DESC LIMIT ?3"
    };
    let mut statement = connection.prepare(sql)?;
    let first: rusqlite::types::Value = if by_name {
        after.name.clone().into()
    } else {
        after.updated.into()
    };
    let rows = statement.query_map(params![first, after.thread, limit as u32], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
            row.get(5)?,
        ))
    })?;
    let mut bytes = 0usize;
    let mut result = Vec::new();
    for row in rows {
        let (id, name, intent, lifecycle, parent, updated) = row?;
        bytes = bytes.saturating_add(name.len() + intent.len());
        if bytes > 4 * 1024 * 1024 {
            return Err(Error::Invalid(
                "Thread list summary exceeds byte budget".into(),
            ));
        }
        result.push(Row {
            thread: hash(&id)?,
            name,
            intent,
            lifecycle,
            parent: parent.as_deref().map(hash).transpose()?,
            updated,
        });
    }
    Ok(result)
}

/// Constant-size durable fence independent of watcher delivery latency.
pub fn epoch(heddle_dir: &Path) -> Result<i64> {
    let path = heddle_dir.join("thread-replication.sqlite3");
    if !path.exists() {
        return Ok(0);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(db.query_row(
        "SELECT version FROM thread_list_epoch WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
#[cfg(test)]
mod tests {
    use crypto::{
        Ed25519Signer, Signer,
        thread_operation::{SignedGenesis, SignedOperation},
    };
    use objects::object::{
        CollaborationActor, StateId,
        thread_replication::{ThreadOperation, metadata::AUTHORITY_FORMAT},
    };

    use super::*;
    use crate::thread_replication::ThreadReplica;
    #[test]
    fn list_index_tracks_causal_conflicts_resolution_and_exact_replay() {
        let dir = tempfile::tempdir().expect("repo");
        let repo = crate::Repository::init_default(dir.path()).expect("repo");
        let signer = Ed25519Signer::from_seed(&[121; 32]).expect("key");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(7).to_string(),
            parent: None,
            base: repo.head().expect("head").expect("state"),
            name: "original".into(),
            intent: "goal".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![7],
        };
        let replica = ThreadReplica::create(
            repo.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("replica");
        let start = epoch(repo.heddle_dir()).expect("epoch");
        let make = |id: u128, name: &str, parents| {
            let envelope = b"already independently admitted fixture";
            let control = ThreadControl {
                version: 1,
                spool: uuid::Uuid::from_u128(7),
                actor: CollaborationActor {
                    principal_id: uuid::Uuid::from_u128(9),
                    agent_id: None,
                },
                authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, envelope),
                authority_envelope: envelope.to_vec(),
                client_operation_id: uuid::Uuid::from_u128(id),
                occurred_at_ms: 1,
                control: Control::Name(name.into()),
            };
            SignedOperation::sign(
                &ThreadOperation {
                    version: 1,
                    thread: replica.thread_id(),
                    publisher: signer.public_key().try_into().expect("key"),
                    parents,
                    body: ThreadOperationBody::Metadata(control.encode().expect("control")),
                },
                &signer,
            )
            .expect("signature")
        };
        let first = make(1, "first", Default::default());
        replica
            .receive(&first, repo.store(), |_| Ok(()))
            .expect("admit");
        assert_eq!(
            page(repo.heddle_dir(), true, None, 2)
                .expect("page")
                .into_iter()
                .find(|row| row.thread == replica.thread_id())
                .expect("requested Thread")
                .name,
            "first"
        );
        assert!(epoch(repo.heddle_dir()).expect("epoch") > start);
        let second = make(2, "second", Default::default());
        replica
            .receive(&second, repo.store(), |_| Ok(()))
            .expect("concurrent");
        assert_eq!(
            page(repo.heddle_dir(), true, None, 2)
                .expect("page")
                .into_iter()
                .find(|row| row.thread == replica.thread_id())
                .expect("requested Thread")
                .name,
            "original",
            "concurrent candidates do not choose arrival order"
        );
        let resolved = make(
            3,
            "resolved",
            [
                first.verify().expect("proof").id().expect("id"),
                second.verify().expect("proof").id().expect("id"),
            ]
            .into_iter()
            .collect(),
        );
        replica
            .receive_control_cas(&resolved, repo.store(), |_| Ok(()))
            .expect("resolve");
        let version = epoch(repo.heddle_dir()).expect("epoch");
        assert_eq!(
            page(repo.heddle_dir(), true, None, 2)
                .expect("page")
                .into_iter()
                .find(|row| row.thread == replica.thread_id())
                .expect("requested Thread")
                .name,
            "resolved"
        );
        replica
            .receive(&resolved, repo.store(), |_| Ok(()))
            .expect("retry");
        assert_eq!(epoch(repo.heddle_dir()).expect("same epoch"), version);
    }
    #[test]
    fn name_and_updated_pages_seek_deep_without_reading_history() {
        let dir = tempfile::tempdir().expect("replica");
        let signer = Ed25519Signer::from_seed(&[122; 32]).expect("key");
        let genesis = ThreadGenesis {
            version: 1,
            spool: uuid::Uuid::from_u128(1).to_string(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "zzzz".into(),
            intent: "index fixture".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![1],
        };
        let replica = ThreadReplica::create(
            dir.path(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
        )
        .expect("replica");
        let mut connection = replica.connect().expect("connection");
        let tx = connection.transaction().expect("tx");
        for n in 0..10_032u64 {
            let mut id = [0u8; 32];
            id[24..].copy_from_slice(&n.to_be_bytes());
            tx.execute("INSERT INTO thread_list(thread,name,intent,lifecycle,updated) VALUES(?1,?2,'',1,?3)",params![id,format!("name-{n:05}"),n as i64]).expect("summary");
        }
        tx.commit().expect("commit");
        let cursor = Cursor {
            name: "name-09999".into(),
            updated: 9999,
            thread: vec![255; 32],
        };
        let rows = page(dir.path(), true, Some(&cursor), 16).expect("name page");
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0].name, "name-10000");
        let rows = page(dir.path(), false, Some(&cursor), 16).expect("updated page");
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0].updated, 9999);
        for (sql, first) in [
            (
                "SELECT thread FROM thread_list WHERE (name,thread)>(?1,?2) ORDER BY name,thread LIMIT 16",
                rusqlite::types::Value::Text(cursor.name.clone()),
            ),
            (
                "SELECT thread FROM thread_list WHERE (updated,thread)<(?1,?2) ORDER BY updated DESC,thread DESC LIMIT 16",
                rusqlite::types::Value::Integer(cursor.updated),
            ),
        ] {
            let mut statement = connection
                .prepare(sql)
                .expect("indexed production ordering");
            let rows = statement
                .query_map(params![first, cursor.thread], |r| r.get::<_, Vec<u8>>(0))
                .expect("query")
                .collect::<std::result::Result<Vec<_>, _>>()
                .expect("rows");
            assert_eq!(rows.len(), 16);
            let steps = statement.get_status(rusqlite::StatementStatus::VmStep);
            assert!(steps < 400, "deep16 row page used {steps} VM instructions");
        }
    }
}
