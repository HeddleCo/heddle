//! Bounded, rebuildable native source-search projection. Extraction runs before
//! this short SQLite transaction; candidate bytes never become authority.
use objects::object::{ContentHash, StateId};
use rusqlite::{TransactionBehavior, params};

use super::{Error, Result};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS source_search_candidates(
 candidate BLOB NOT NULL UNIQUE CHECK(length(candidate)=32),
 operation BLOB NOT NULL CHECK(length(operation)=32),
 thread BLOB NOT NULL CHECK(length(thread)=32),
 revision BLOB NOT NULL CHECK(length(revision)=32),
 kind INTEGER NOT NULL CHECK(kind IN(3,4)),
 path TEXT NOT NULL, symbol_id TEXT NOT NULL, symbol_name TEXT NOT NULL,
 start_line INTEGER, end_line INTEGER,
 CHECK(length(path)<=4096 AND length(symbol_id)<=4096 AND length(symbol_name)<=4096)
);
CREATE INDEX IF NOT EXISTS source_search_candidates_operation ON source_search_candidates(operation);
CREATE VIRTUAL TABLE IF NOT EXISTS source_search_fts USING fts5(text,tokenize='unicode61');
CREATE TABLE IF NOT EXISTS source_search_ready(
 operation BLOB PRIMARY KEY CHECK(length(operation)=32),
 revision BLOB NOT NULL CHECK(length(revision)=32),
 content_ready INTEGER NOT NULL CHECK(content_ready IN(0,1)),
 symbols_ready INTEGER NOT NULL CHECK(symbols_ready IN(0,1))
);";

/// One location and bounded text prepared outside the metadata transaction.
#[derive(Clone, Debug)]
pub struct Document {
    pub kind: i32,
    pub path: String,
    pub symbol_id: String,
    pub symbol_name: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub text: String,
}

/// Replace one accepted original's index atomically. Source Search verifies
/// that the operation remains accepted and visible at query time.
pub fn publish(
    directory: &std::path::Path,
    thread: ContentHash,
    operation: ContentHash,
    revision: StateId,
    documents: &[Document],
    content_ready: bool,
    symbols_ready: bool,
) -> Result<()> {
    if documents.len() > 8192
        || documents.iter().any(|document| {
            !matches!(document.kind, 3 | 4)
                || document.path.len() > 4096
                || document.symbol_id.len() > 4096
                || document.symbol_name.len() > 4096
                || document.text.len() > 65536
        })
    {
        return Err(Error::Invalid(
            "source search projection exceeds bounds".into(),
        ));
    }
    let mut connection = crate::local_metadata::open(directory)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accepted: i64 = transaction.query_row(
        "SELECT count(*) FROM operations WHERE id=?1 AND thread=?2 AND source_revision=?3 AND facet=1 AND status=1",
        params![operation.as_bytes(), thread.as_bytes(), revision.as_bytes()],
        |row| row.get(0),
    )?;
    if accepted != 1 {
        return Err(Error::Invalid(
            "source search original is not accepted".into(),
        ));
    }
    transaction.execute(
        "DELETE FROM source_search_fts WHERE rowid IN (SELECT rowid FROM source_search_candidates WHERE operation=?1)",
        [operation.as_bytes().as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM source_search_candidates WHERE operation=?1",
        [operation.as_bytes().as_slice()],
    )?;
    for document in documents {
        let mut digest = blake3::Hasher::new_derive_key("heddle-native-source-search-candidate-v1");
        digest.update(operation.as_bytes());
        digest.update(&document.kind.to_be_bytes());
        digest.update(document.path.as_bytes());
        digest.update(document.symbol_id.as_bytes());
        let candidate = digest.finalize();
        transaction.execute(
            "INSERT INTO source_search_candidates(candidate,operation,thread,revision,kind,path,symbol_id,symbol_name,start_line,end_line) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![candidate.as_bytes(),operation.as_bytes(),thread.as_bytes(),revision.as_bytes(),document.kind,document.path,document.symbol_id,document.symbol_name,document.start_line,document.end_line],
        )?;
        let rowid = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO source_search_fts(rowid,text) VALUES(?1,?2)",
            params![rowid, document.text],
        )?;
    }
    transaction.execute(
        "INSERT INTO source_search_ready(operation,revision,content_ready,symbols_ready) VALUES(?1,?2,?3,?4) ON CONFLICT(operation) DO UPDATE SET revision=excluded.revision,content_ready=excluded.content_ready,symbols_ready=excluded.symbols_ready",
        params![operation.as_bytes(),revision.as_bytes(),content_ready,symbols_ready],
    )?;
    transaction.execute(
        "INSERT INTO metadata_changes(topic,entity) VALUES('source_search',?1)",
        [operation.to_string()],
    )?;
    transaction.commit()?;
    objects::fs_atomic::write_file_atomic(
        &directory.join(crate::local_metadata::CHANGE_MARKER_NAME),
        uuid::Uuid::new_v4().as_bytes(),
    )?;
    Ok(())
}
