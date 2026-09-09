//! Rebuildable full-text and typed property projections of original collaboration records.
use objects::object::{
    CollaborationOperationBodyV1 as Body, CollaborationOperationEnvelope,
    thread_replication::ThreadOperationBody,
};
use rusqlite::{Transaction, params};

use super::{Error, Result};

pub(super) const SCHEMA: &str = "CREATE VIRTUAL TABLE IF NOT EXISTS collaboration_search USING fts5(operation UNINDEXED,thread UNINDEXED,kind UNINDEXED,record UNINDEXED,text,tokenize='unicode61'); CREATE TABLE IF NOT EXISTS collaboration_search_indexed(operation BLOB PRIMARY KEY) WITHOUT ROWID;";

pub(super) fn index(
    tx: &Transaction<'_>,
    operation: &objects::object::thread_replication::ThreadOperation,
) -> Result<()> {
    if !matches!(
        operation.body,
        ThreadOperationBody::Discussion(_) | ThreadOperationBody::Context(_)
    ) {
        return Ok(());
    }
    let id = operation.id()?;
    if tx.execute(
        "INSERT OR IGNORE INTO collaboration_search_indexed(operation) VALUES(?1)",
        [id.as_bytes().as_slice()],
    )? == 0
    {
        return Ok(());
    }
    if let Some(context) = operation.context_revision()? {
        crate::reference_projection::project_properties(
            tx,
            &context.metadata.scope,
            id,
            &context.tags,
        )
        .map_err(|error| Error::Invalid(error.to_string()))?;
        tx.execute("INSERT INTO collaboration_search(operation,thread,kind,record,text) VALUES(?1,?2,2,?3,?4)", params![id.as_bytes().as_slice(), operation.thread.as_bytes().as_slice(), context.id.to_string(), context.content])?;
    }
    if let ThreadOperationBody::Discussion(bytes) = &operation.body {
        let decoded = CollaborationOperationEnvelope::decode(bytes)
            .map_err(|error| Error::Invalid(error.to_string()))?;
        let text = match &decoded.operation.body {
            Body::Open { title, turn, .. } => format!("{title}\n{}", turn.body),
            Body::AppendTurn { turn } => turn.body.clone(),
            Body::Reopen { reason } => reason.clone(),
            _ => String::new(),
        };
        if !text.is_empty() {
            tx.execute("INSERT INTO collaboration_search(operation,thread,kind,record,text) VALUES(?1,?2,1,?3,?4)", params![id.as_bytes().as_slice(), operation.thread.as_bytes().as_slice(), decoded.operation.discussion_id.to_string(), text])?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub thread: objects::object::ContentHash,
    pub operation: objects::object::ContentHash,
    pub kind: i32,
    pub record: String,
    pub snippet: String,
    pub score: f64,
}

/// Literal phrase search does not accept FTS syntax from the wire. Canonical
/// pending/rejected operations and superseded context revisions are excluded.
pub fn search(
    directory: &std::path::Path,
    text: &str,
    offset: u32,
    limit: u32,
) -> Result<Vec<Hit>> {
    if text.trim().is_empty() || text.len() > 4096 || limit == 0 || limit > 257 || offset > 10_000 {
        return Err(Error::Invalid(
            "invalid full-text query or page bound".into(),
        ));
    }
    let connection = crate::local_metadata::open_existing(directory)?
        .ok_or_else(|| Error::Invalid("local metadata missing".into()))?;
    bound_query_work(&connection)?;
    let phrase = format!("\"{}\"", text.trim().replace('"', "\"\""));
    let mut query = connection.prepare("SELECT s.thread,s.operation,s.kind,s.record,snippet(collaboration_search,4,'','',' … ',32),bm25(collaboration_search) AS score FROM collaboration_search s JOIN operations o ON o.id=s.operation WHERE collaboration_search MATCH ?1 AND o.status=1 AND (s.kind<>2 OR NOT EXISTS(SELECT 1 FROM parents p JOIN operations child ON child.id=p.child JOIN collaboration_operations c ON c.operation=child.id WHERE p.parent=o.id AND child.status=1 AND c.thread=s.thread AND c.record_kind=2 AND c.record_id=s.record)) ORDER BY score,s.thread,s.operation LIMIT ?2 OFFSET ?3")?;
    let rows = query.query_map(params![phrase, limit, offset], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;
    rows.map(|row| {
        let (thread, operation, kind, record, snippet, score) = row?;
        Ok(Hit {
            thread: super::hash(&thread)?,
            operation: super::hash(&operation)?,
            kind,
            record,
            snippet,
            score,
        })
    })
    .collect()
}

fn bound_query_work(connection: &rusqlite::Connection) -> rusqlite::Result<()> {
    // Sorting by rank can inspect more rows than LIMIT returns. Interrupt the
    // SQLite VM itself so a common term cannot occupy a worker indefinitely.
    let started = std::time::Instant::now();
    let mut checkpoints = 0u32;
    connection.progress_handler(
        1000,
        Some(move || {
            checkpoints = checkpoints.saturating_add(1);
            checkpoints > 10_000 || started.elapsed() > std::time::Duration::from_secs(2)
        }),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_work_interrupts_large_rank_equivalent_scans() {
        let connection = rusqlite::Connection::open_in_memory().expect("query database");
        super::bound_query_work(&connection).expect("install work ceiling");
        let result = connection.query_row(
            "WITH RECURSIVE rows(n) AS (VALUES(0) UNION ALL SELECT n+1 FROM rows WHERE n<3000000) SELECT sum(n) FROM rows",
            [], |row| row.get::<_,i64>(0),
        );
        assert!(
            matches!(result, Err(rusqlite::Error::SqliteFailure(ref error, _)) if error.code == rusqlite::ErrorCode::OperationInterrupted),
            "query work ceiling must interrupt execution, not merely truncate result rows: {result:?}"
        );
    }
}
