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
    pub cursor: objects::object::ContentHash,
    pub kind: i32,
    pub record: String,
    pub snippet: String,
    pub score: f64,
    pub revision: Option<objects::object::StateId>,
    pub path: String,
    pub symbol_id: String,
    pub symbol_name: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
}

pub struct NativeBatch {
    pub hits: Vec<Hit>,
    /// Rows consumed before caller visibility filtering; this drives the cursor.
    pub scanned: usize,
}

/// Source-domain selection only. Revision identifier hits remain historical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceSelection {
    Current,
    Retained,
    Exact(objects::object::StateId),
}

/// Bounded current-record candidates. Search text is literal. The caller
/// projects and evaluates typed tags on each exact signed context revision
/// after its authorization check.
#[cfg(test)]
pub(crate) fn search_native(
    directory: &std::path::Path,
    text: &str,
    after_operation: Option<objects::object::ContentHash>,
    limit: u32,
    kinds: &[i32],
    annotations: Option<&objects::object::AnnotationQuery>,
    source: SourceSelection,
) -> Result<NativeBatch> {
    search_native_inner(directory, text, after_operation, limit, kinds, annotations, source, None)
}

/// Search with a query-local set of exact source paths admitted by the caller's
/// signed Thread/source and selected-leaf gates. Source rows are joined to this
/// set before ORDER BY/LIMIT, so hidden rows cannot consume page slots.
pub fn search_native_admitted(
    directory: &std::path::Path,
    text: &str,
    after_operation: Option<objects::object::ContentHash>,
    limit: u32,
    kinds: &[i32],
    annotations: Option<&objects::object::AnnotationQuery>,
    source: SourceSelection,
    admitted: &[super::source_search::IndexedSourcePath],
) -> Result<NativeBatch> {
    search_native_inner(directory, text, after_operation, limit, kinds, annotations, source, Some(admitted))
}

fn search_native_inner(
    directory: &std::path::Path,
    text: &str,
    after_operation: Option<objects::object::ContentHash>,
    limit: u32,
    kinds: &[i32],
    annotations: Option<&objects::object::AnnotationQuery>,
    source: SourceSelection,
    admitted: Option<&[super::source_search::IndexedSourcePath]>,
) -> Result<NativeBatch> {
    if text.len() > 4096
        || limit == 0
        || limit > 257
        || kinds.is_empty()
        || kinds.len() > 6
        || kinds.iter().any(|kind| !matches!(kind, 0..=5))
        || (text.trim().is_empty() && annotations.is_none())
        || (annotations.is_some() && kinds != [2])
    {
        return Err(Error::Invalid(
            "invalid native search query or page bound".into(),
        ));
    }
    let connection = crate::local_metadata::open_existing(directory)?
        .ok_or_else(|| Error::Invalid("local metadata missing".into()))?;
    bound_query_work(&connection)?;
    connection.execute_batch("CREATE TEMP TABLE admitted_source_paths(thread BLOB NOT NULL,revision BLOB NOT NULL,path TEXT NOT NULL,PRIMARY KEY(thread,revision,path)) WITHOUT ROWID")?;
    if let Some(admitted) = admitted {
        let mut insert = connection.prepare("INSERT OR IGNORE INTO admitted_source_paths(thread,revision,path) VALUES(?1,?2,?3)")?;
        for item in admitted {
            insert.execute(rusqlite::params![item.thread.as_bytes(),item.revision.as_bytes(),item.path])?;
        }
    }
    let phrase = format!("\"{}\"", text.trim().replace('"', "\"\""));
    let exact_revision = text.trim().strip_prefix("heddle:").unwrap_or(text.trim());
    let exact_revision = objects::object::StateId::parse(exact_revision).ok();
    // Source scores stay corpus-independent: FTS bm25 includes hidden documents
    // in its collection statistics and would expose their presence through an
    // otherwise visible hit's score/order before the device visibility gate.
    let lexical = "WITH hits AS (
        SELECT s.thread,s.operation,s.kind,s.record,
               snippet(collaboration_search,4,'','',' … ',32) summary,
               bm25(collaboration_search) score,o.canonical,s.operation cursor,
               NULL revision,'' path,'' symbol_id,'' symbol_name,NULL start_line,NULL end_line
        FROM collaboration_search s JOIN operations o ON o.id=s.operation
        WHERE collaboration_search MATCH ?1 AND o.status=1
          AND ((s.kind=1 AND ?2) OR (s.kind=2 AND ?3))
          AND (s.kind<>2 OR NOT EXISTS(
            SELECT 1 FROM parents p JOIN operations child ON child.id=p.child
            JOIN collaboration_operations c ON c.operation=child.id
            WHERE p.parent=o.id AND child.status=1 AND c.thread=s.thread
              AND c.record_kind=2 AND c.record_id=s.record))
        UNION ALL
        SELECT l.thread,l.thread,0,lower(hex(l.thread)),
               substr(l.name||char(10)||l.intent,1,512),-1.0,x'',l.thread,
               NULL,'','','',NULL,NULL
        FROM thread_list l WHERE ?4 AND instr(lower(l.name||' '||l.intent),lower(?5))>0
        UNION ALL
        SELECT o.thread,o.id,5,?9,?9,-2.0,o.canonical,o.id,
               o.source_revision,'','','',NULL,NULL
        FROM operations o WHERE ?8 AND o.status=1 AND o.facet=1 AND o.source_revision=?10
        UNION ALL
        SELECT c.thread,c.operation,c.kind,lower(hex(c.candidate)),
               snippet(source_search_fts,0,'','',' … ',32),-3.0,o.canonical,c.candidate,
               c.revision,c.path,c.symbol_id,c.symbol_name,c.start_line,c.end_line
        FROM source_search_fts JOIN source_search_candidates c ON c.rowid=source_search_fts.rowid
        JOIN operations o ON o.id=c.operation
        JOIN source_search_ready ready ON ready.operation=c.operation AND ready.revision=c.revision AND ready.extractor_version=1
        WHERE source_search_fts MATCH ?1 AND o.status=1 AND o.facet=1
          AND o.thread=c.thread AND o.source_revision=c.revision
          AND ((c.kind=3 AND ?11) OR (c.kind=4 AND ?12))
          AND (?13 OR EXISTS(SELECT 1 FROM thread_source_head_revisions head WHERE head.thread=c.thread AND head.revision=c.revision))
          AND (?14 IS NULL OR c.revision=?14)
          AND (?15=0 OR EXISTS(SELECT 1 FROM admitted_source_paths a
            WHERE a.thread=c.thread AND a.revision=c.revision AND a.path=c.path))
      ), anchor AS (
        SELECT score,thread,cursor,kind,record FROM hits WHERE cursor=?6
      ) SELECT thread,operation,kind,record,summary,score,canonical,cursor,revision,path,symbol_id,symbol_name,start_line,end_line FROM hits
        WHERE ?6 IS NULL OR ((SELECT count(*) FROM anchor)=1 AND
          (score,thread,cursor,kind,record)>(SELECT score,thread,cursor,kind,record FROM anchor))
        ORDER BY score,thread,cursor,kind,record LIMIT ?7";
    let filters_only = "WITH hits AS (SELECT s.thread,s.operation,s.kind,s.record,
               substr(s.text,1,512) summary,0.0 score,o.canonical,s.operation cursor,
               NULL revision,'' path,'' symbol_id,'' symbol_name,NULL start_line,NULL end_line
        FROM collaboration_search s JOIN operations o ON o.id=s.operation
        WHERE o.status=1 AND s.kind=2 AND NOT EXISTS(
            SELECT 1 FROM parents p JOIN operations child ON child.id=p.child
            JOIN collaboration_operations c ON c.operation=child.id
            WHERE p.parent=o.id AND child.status=1 AND c.thread=s.thread
              AND c.record_kind=2 AND c.record_id=s.record)
      ), anchor AS (
        SELECT score,thread,cursor,kind,record FROM hits WHERE cursor=?1
      ) SELECT thread,operation,kind,record,summary,score,canonical,cursor,revision,path,symbol_id,symbol_name,start_line,end_line FROM hits
        WHERE ?1 IS NULL OR ((SELECT count(*) FROM anchor)=1 AND
          (score,thread,cursor,kind,record)>(SELECT score,thread,cursor,kind,record FROM anchor))
        ORDER BY score,thread,cursor,kind,record LIMIT ?2";
    let mut statement = connection.prepare(if text.trim().is_empty() {
        filters_only
    } else {
        lexical
    })?;
    type Row = (
        Vec<u8>,
        Vec<u8>,
        i32,
        String,
        String,
        f64,
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
        String,
        String,
        String,
        Option<u32>,
        Option<u32>,
    );
    let read = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Row> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
        ))
    };
    let rows = if text.trim().is_empty() {
        statement.query_map(
            params![after_operation.map(|id| id.as_bytes().to_vec()), limit],
            read,
        )?
    } else {
        statement.query_map(
            params![
                phrase,
                kinds.contains(&1),
                kinds.contains(&2),
                kinds.contains(&0),
                text.trim(),
                after_operation.map(|id| id.as_bytes().to_vec()),
                limit,
                kinds.contains(&5),
                text.trim(),
                exact_revision.map(|id| id.as_bytes().to_vec()),
                kinds.contains(&3),
                kinds.contains(&4),
                !matches!(source, SourceSelection::Current),
                match source {
                    SourceSelection::Exact(revision) => Some(revision.as_bytes().to_vec()),
                    _ => None,
                },
                admitted.is_some(),
            ],
            read,
        )?
    };
    let mut hits = Vec::new();
    let mut scanned = 0;
    for row in rows {
        let (
            thread,
            operation,
            kind,
            record,
            summary,
            score,
            _canonical,
            cursor,
            revision,
            path,
            symbol_id,
            symbol_name,
            start_line,
            end_line,
        ) = row?;
        scanned += 1;
        hits.push(Hit {
            thread: super::hash(&thread)?,
            operation: super::hash(&operation)?,
            cursor: super::hash(&cursor)?,
            kind,
            record,
            snippet: summary,
            score,
            revision: revision
                .map(|value| {
                    super::hash(&value)
                        .map(|hash| objects::object::StateId::from_content_hash(hash))
                })
                .transpose()?,
            path,
            symbol_id,
            symbol_name,
            start_line,
            end_line,
        });
    }
    Ok(NativeBatch { hits, scanned })
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
            cursor: super::hash(&operation)?,
            kind,
            record,
            snippet,
            score,
            revision: None,
            path: String::new(),
            symbol_id: String::new(),
            symbol_name: String::new(),
            start_line: None,
            end_line: None,
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
