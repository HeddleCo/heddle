//! Bounded, rebuildable native source-search projection. Extraction runs before
//! this short SQLite transaction; candidate bytes never become authority.
use objects::object::{ContentHash, StateId};
use rusqlite::{TransactionBehavior, params};

use super::{Error, Result};

/// One indexed source target. It is a search candidate, never read authority;
/// callers must verify its exact Thread and signed source before admission.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct IndexedSourceTarget {
    /// Indexed owning Thread; callers must authorize it independently.
    pub thread: ContentHash,
    /// Accepted source State named by the original operation.
    pub revision: StateId,
}

/// One short-lived read connection for an exact source Search snapshot. It
/// holds no transaction while signed source proofs are evaluated.
pub struct SourceSearchReader {
    connection: rusqlite::Connection,
}

/// Readiness for an already authorized exact source target. Every accepted
/// original of the same State must have the current extractor projection;
/// zero-document sources are complete only with an explicit ready/count row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceIndexReadiness {
    /// All accepted originals have complete content extraction and intact leaf proofs.
    pub content: bool,
    /// All accepted originals have complete symbol extraction and intact leaf proofs.
    pub symbols: bool,
}

impl SourceSearchReader {
    /// Open the existing local metadata projection once for the query page.
    pub fn open(directory: &std::path::Path) -> Result<Self> {
        let connection = crate::local_metadata::open_existing(directory)?
            .ok_or_else(|| Error::Invalid("local metadata missing".into()))?;
        Ok(Self { connection })
    }

    /// Visit accepted targets in bounded keyset batches without retaining a
    /// statement while the caller checks signed source and visibility facts.
    pub fn visit_indexed_targets(
        &self,
        source: super::collaboration_search::SourceSelection,
        selected_threads: Option<&[ContentHash]>,
        mut visit: impl FnMut(IndexedSourceTarget) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
    let connection = &self.connection;
    connection.execute_batch("CREATE TEMP TABLE IF NOT EXISTS selected_source_threads(thread BLOB PRIMARY KEY) WITHOUT ROWID;
        DELETE FROM selected_source_threads")?;
    if let Some(threads) = selected_threads {
        let mut insert = connection.prepare("INSERT OR IGNORE INTO selected_source_threads(thread) VALUES(?1)")?;
        for thread in threads {
            insert.execute([thread.as_bytes().as_slice()])?;
        }
    }
    let mut after: Option<IndexedSourceTarget> = None;
    loop {
        let page = {
            let selected_join = if selected_threads.is_some() {
                "JOIN selected_source_threads selected ON selected.thread=o.thread"
            } else {
                ""
            };
            let query = format!(
                "SELECT DISTINCT o.thread,o.source_revision FROM operations o {selected_join}
                 WHERE o.status=1 AND o.facet=1 AND o.source_revision IS NOT NULL
                   AND (?1 OR EXISTS(SELECT 1 FROM thread_source_head_revisions h
                    WHERE h.thread=o.thread AND h.revision=o.source_revision))
                   AND (?2 IS NULL OR o.source_revision=?2)
                   AND (?3 IS NULL OR (o.thread,o.source_revision)>(?3,?4))
                 ORDER BY o.thread,o.source_revision LIMIT 256",
            );
            let mut statement = connection.prepare(&query)?;
            let exact = match source {
                super::collaboration_search::SourceSelection::Exact(id) => Some(id),
                _ => None,
            };
            let rows = statement.query_map(
                params![
                    matches!(source, super::collaboration_search::SourceSelection::Retained | super::collaboration_search::SourceSelection::Exact(_)),
                    exact.as_ref().map(StateId::as_bytes),
                    after.as_ref().map(|value| value.thread.as_bytes().as_slice()),
                    after.as_ref().map(|value| value.revision.as_bytes().as_slice()),
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )?;
            rows.map(|row| {
                let (thread, revision) = row?;
                Ok(IndexedSourceTarget {
                    thread: super::hash(&thread)?,
                    revision: StateId::from_bytes(*super::hash(&revision)?.as_bytes()),
                })
            }).collect::<Result<Vec<_>>>()?
        };
        if page.is_empty() {
            return Ok(());
        }
        let more = page.len() == 256;
        after = page.last().cloned();
        for candidate in page {
            visit(candidate)?;
        }
        if !more {
            return Ok(());
        }
    }
}

    /// Check current extractor completion for one previously admitted source.
    pub fn readiness_for_authorized_target(
        &self,
        thread: ContentHash,
        revision: StateId,
    ) -> Result<SourceIndexReadiness> {
    let connection = &self.connection;
    let (content, symbols): (i64, i64) = connection.query_row(
        "SELECT COALESCE(MIN(CASE WHEN r.extractor_version=3 AND r.revision=o.source_revision
            AND r.content_ready=1 AND counts.content_count=(
                SELECT count(*) FROM source_search_candidates c WHERE c.operation=o.id AND c.kind=3)
            AND NOT EXISTS(SELECT 1 FROM source_search_candidates c
                LEFT JOIN source_search_candidate_proofs p ON p.candidate=c.candidate
                WHERE c.operation=o.id AND c.kind=3 AND (p.candidate IS NULL OR p.leaf_count<>(
                    SELECT count(*) FROM source_search_candidate_leaves leaves WHERE leaves.candidate=c.candidate)))
            THEN 1 ELSE 0 END),0),
          COALESCE(MIN(CASE WHEN r.extractor_version=3 AND r.revision=o.source_revision
            AND r.symbols_ready=1 AND counts.symbol_count=(
                SELECT count(*) FROM source_search_candidates c WHERE c.operation=o.id AND c.kind=4)
            AND NOT EXISTS(SELECT 1 FROM source_search_candidates c
                LEFT JOIN source_search_candidate_proofs p ON p.candidate=c.candidate
                WHERE c.operation=o.id AND c.kind=4 AND (p.candidate IS NULL OR p.leaf_count<>(
                    SELECT count(*) FROM source_search_candidate_leaves leaves WHERE leaves.candidate=c.candidate)))
            THEN 1 ELSE 0 END),0)
         FROM operations o
         LEFT JOIN source_search_ready r ON r.operation=o.id
         LEFT JOIN source_search_counts counts ON counts.operation=o.id
         WHERE o.thread=?1 AND o.source_revision=?2 AND o.facet=1 AND o.status=1",
        params![thread.as_bytes(), revision.as_bytes()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(SourceIndexReadiness {
        content: content == 1,
        symbols: symbols == 1,
    })
}
}

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
CREATE INDEX IF NOT EXISTS source_search_candidates_target ON source_search_candidates(thread,revision);
CREATE TABLE IF NOT EXISTS source_search_candidate_leaves(
 candidate BLOB NOT NULL CHECK(length(candidate)=32),
 leaf BLOB NOT NULL CHECK(length(leaf)=32),
 PRIMARY KEY(candidate,leaf)
);
CREATE TABLE IF NOT EXISTS source_search_candidate_proofs(
 candidate BLOB PRIMARY KEY CHECK(length(candidate)=32),
 leaf_count INTEGER NOT NULL CHECK(leaf_count BETWEEN 0 AND 128)
);
CREATE VIRTUAL TABLE IF NOT EXISTS source_search_fts USING fts5(text,tokenize='unicode61');
CREATE TABLE IF NOT EXISTS source_search_ready(
 operation BLOB PRIMARY KEY CHECK(length(operation)=32),
 revision BLOB NOT NULL CHECK(length(revision)=32),
 extractor_version INTEGER NOT NULL CHECK(extractor_version>=1),
 content_ready INTEGER NOT NULL CHECK(content_ready IN(0,1)),
 symbols_ready INTEGER NOT NULL CHECK(symbols_ready IN(0,1))
);
CREATE TABLE IF NOT EXISTS source_search_counts(
 operation BLOB PRIMARY KEY CHECK(length(operation)=32),
 content_count INTEGER NOT NULL CHECK(content_count BETWEEN 0 AND 8192),
 symbol_count INTEGER NOT NULL CHECK(symbol_count BETWEEN 0 AND 8192)
);
CREATE INDEX IF NOT EXISTS source_search_ready_version ON source_search_ready(extractor_version,operation);
CREATE TABLE IF NOT EXISTS source_search_queue(
 operation BLOB PRIMARY KEY CHECK(length(operation)=32),
 thread BLOB NOT NULL CHECK(length(thread)=32),
 revision BLOB NOT NULL CHECK(length(revision)=32),
 next_attempt INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS source_search_queue_due ON source_search_queue(next_attempt,operation);
CREATE TABLE IF NOT EXISTS source_search_bootstrap(version INTEGER PRIMARY KEY CHECK(version>=1));
INSERT OR IGNORE INTO source_search_queue(operation,thread,revision)
SELECT o.id,o.thread,o.source_revision FROM operations o
WHERE o.status=1 AND o.facet=1
  AND NOT EXISTS(SELECT 1 FROM source_search_ready r WHERE r.operation=o.id AND r.revision=o.source_revision AND r.extractor_version=3)
  AND NOT EXISTS(SELECT 1 FROM source_search_bootstrap WHERE version=3);
INSERT OR IGNORE INTO source_search_bootstrap(version) VALUES(3);
INSERT OR IGNORE INTO source_search_queue(operation,thread,revision)
SELECT o.id,o.thread,o.source_revision FROM source_search_ready r
JOIN operations o ON o.id=r.operation AND o.status=1 AND o.facet=1
WHERE r.extractor_version<>3;
CREATE TRIGGER IF NOT EXISTS source_search_admitted_update AFTER UPDATE OF status ON operations
WHEN OLD.status<>1 AND NEW.status=1 AND NEW.facet=1
BEGIN
 INSERT OR IGNORE INTO source_search_queue(operation,thread,revision) VALUES(NEW.id,NEW.thread,NEW.source_revision);
END;
CREATE TRIGGER IF NOT EXISTS source_search_admitted_insert AFTER INSERT ON operations
WHEN NEW.status=1 AND NEW.facet=1
BEGIN
 INSERT OR IGNORE INTO source_search_queue(operation,thread,revision) VALUES(NEW.id,NEW.thread,NEW.source_revision);
END;";

/// One accepted original awaiting extraction. This is work scheduling, never read authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueuedOriginal {
    pub operation: ContentHash,
    pub thread: ContentHash,
    pub revision: StateId,
}

/// Return a bounded due batch without holding a transaction across parsing.
pub fn due(directory: &std::path::Path, now: i64, limit: usize) -> Result<Vec<QueuedOriginal>> {
    if !(1..=32).contains(&limit) {
        return Err(Error::Invalid(
            "source search queue batch must be 1..32".into(),
        ));
    }
    let connection = crate::local_metadata::open(directory)?;
    let mut statement = connection.prepare("SELECT q.operation,q.thread,q.revision FROM source_search_queue q JOIN operations o ON o.id=q.operation AND o.status=1 AND o.facet=1 WHERE q.next_attempt<=?1 ORDER BY q.next_attempt,q.operation LIMIT ?2")?;
    let rows = statement.query_map(params![now, limit as i64], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (operation, thread, revision) = row?;
        Ok(QueuedOriginal {
            operation: super::hash(&operation)?,
            thread: super::hash(&thread)?,
            revision: StateId::from_bytes(*super::hash(&revision)?.as_bytes()),
        })
    })
    .collect()
}

/// Defer an unavailable source without pinning progress behind that queue row.
pub fn defer(directory: &std::path::Path, operation: ContentHash, next_attempt: i64) -> Result<()> {
    let connection = crate::local_metadata::open(directory)?;
    connection.execute(
        "UPDATE source_search_queue SET next_attempt=?2 WHERE operation=?1",
        params![operation.as_bytes(), next_attempt],
    )?;
    Ok(())
}

/// Earliest retry time for a daemon-owned maintenance wake.
pub fn next_due(directory: &std::path::Path) -> Result<Option<i64>> {
    let connection = crate::local_metadata::open(directory)?;
    let at = connection.query_row(
        "SELECT MIN(q.next_attempt) FROM source_search_queue q JOIN operations o ON o.id=q.operation AND o.status=1 AND o.facet=1",
        [],
        |row| row.get(0),
    )?;
    Ok(at)
}

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
    /// Salted leaf commitments on the path, including directory ancestors.
    /// Captured during the identity-checked source tree walk by the indexer.
    pub leaf_chain: Vec<ContentHash>,
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
                || document.leaf_chain.len() > 128
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
        "DELETE FROM source_search_candidate_leaves WHERE candidate IN (SELECT candidate FROM source_search_candidates WHERE operation=?1)",
        [operation.as_bytes().as_slice()],
    )?;
    transaction.execute(
        "DELETE FROM source_search_candidate_proofs WHERE candidate IN (SELECT candidate FROM source_search_candidates WHERE operation=?1)",
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
            "INSERT INTO source_search_candidate_proofs(candidate,leaf_count) VALUES(?1,?2)",
            params![candidate.as_bytes(), document.leaf_chain.iter().collect::<std::collections::HashSet<_>>().len() as i64],
        )?;
        transaction.execute(
            "INSERT INTO source_search_fts(rowid,text) VALUES(?1,?2)",
            params![rowid, document.text],
        )?;
        for leaf in &document.leaf_chain {
            transaction.execute(
                "INSERT OR IGNORE INTO source_search_candidate_leaves(candidate,leaf) VALUES(?1,?2)",
                params![candidate.as_bytes(), leaf.as_bytes()],
            )?;
        }
    }
    transaction.execute(
        "INSERT INTO source_search_counts(operation,content_count,symbol_count) VALUES(?1,?2,?3) ON CONFLICT(operation) DO UPDATE SET content_count=excluded.content_count,symbol_count=excluded.symbol_count",
        params![operation.as_bytes(), documents.iter().filter(|document| document.kind==3).count() as i64, documents.iter().filter(|document| document.kind==4).count() as i64],
    )?;
    transaction.execute(
        "INSERT INTO source_search_ready(operation,revision,extractor_version,content_ready,symbols_ready) VALUES(?1,?2,3,?3,?4) ON CONFLICT(operation) DO UPDATE SET revision=excluded.revision,extractor_version=excluded.extractor_version,content_ready=excluded.content_ready,symbols_ready=excluded.symbols_ready",
        params![operation.as_bytes(),revision.as_bytes(),content_ready,symbols_ready],
    )?;
    transaction.execute(
        "INSERT INTO metadata_changes(topic,entity) VALUES('source_search',?1)",
        [operation.to_string()],
    )?;
    if content_ready {
        transaction.execute(
            "DELETE FROM source_search_queue WHERE operation=?1",
            [operation.as_bytes().as_slice()],
        )?;
    } else {
        transaction.execute("UPDATE source_search_queue SET next_attempt=strftime('%s','now')+60 WHERE operation=?1",[operation.as_bytes().as_slice()])?;
    }
    transaction.commit()?;
    objects::fs_atomic::write_file_atomic(
        &directory.join(crate::local_metadata::CHANGE_MARKER_NAME),
        uuid::Uuid::new_v4().as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canceled_target_visit_stops_before_next_batch_and_releases_reader() {
        let root = tempfile::tempdir().expect("temporary metadata");
        let directory = root.path().join(".heddle");
        std::fs::create_dir(&directory).expect("metadata directory");
        let connection = crate::local_metadata::open(&directory).expect("metadata");
        super::super::initialize_schema(&connection).expect("source schema");
        let thread = ContentHash::from_bytes([1; 32]);
        for byte in [2u8, 3] {
            let revision = StateId::from_bytes([byte; 32]);
            let operation = ContentHash::from_bytes([byte + 10; 32]);
            connection.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),1,?3)",
                params![operation.as_bytes(), thread.as_bytes(), revision.as_bytes()])
                .expect("accepted target");
        }
        let reader = SourceSearchReader::open(&directory).expect("reader");
        let mut visited = 0;
        let canceled = reader.visit_indexed_targets(
            super::super::collaboration_search::SourceSelection::Retained,
            Some(&[thread]), |_| {
                visited += 1;
                anyhow::bail!("request canceled")
            });
        assert!(canceled.is_err());
        assert_eq!(visited, 1, "cancellation stops before another target proof");
        let mut resumed = 0;
        reader.visit_indexed_targets(
            super::super::collaboration_search::SourceSelection::Retained,
            Some(&[thread]), |_| { resumed += 1; Ok(()) })
            .expect("reader reusable after canceled visit");
        assert_eq!(resumed, 2);
    }

    #[test]
    fn admitted_paths_filter_hidden_matches_before_page_limit() {
        use super::super::collaboration_search::{AdmittedSourceTarget, SourceSelection, search_native_admitted};
        let root = tempfile::tempdir().expect("temporary metadata");
        let directory = root.path().join(".heddle");
        std::fs::create_dir(&directory).expect("metadata directory");
        let connection = crate::local_metadata::open(&directory).expect("metadata");
        super::super::initialize_schema(&connection).expect("source schema");
        let thread = ContentHash::from_bytes([1; 32]);
        let revision = StateId::from_bytes([2; 32]);
        let operation = ContentHash::from_bytes([3; 32]);
        connection.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),1,?3)", params![operation.as_bytes(),thread.as_bytes(),revision.as_bytes()]).expect("accepted source");
        let document = |path: &str| Document {
            kind: 3,
            path: path.into(),
            symbol_id: String::new(),
            symbol_name: String::new(),
            start_line: None,
            end_line: None,
            text: "alpha".into(),
            leaf_chain: if path == "hidden.rs" { vec![ContentHash::from_bytes([9; 32])] } else { vec![ContentHash::from_bytes([8; 32])] },
        };
        publish(&directory, thread, operation, revision,
            &[document("hidden.rs"), document("visible.rs")], true, false)
            .expect("indexed source");
        let mut indexed = Vec::new();
        let reader = SourceSearchReader::open(&directory).expect("source reader");
        reader.visit_indexed_targets(SourceSelection::Retained, Some(&[thread]), |target| {
            indexed.push(target);
            Ok(())
        }).expect("indexed targets");
        assert_eq!(indexed.len(), 1);
        let mut unrelated = Vec::new();
        reader.visit_indexed_targets(SourceSelection::Retained,
            Some(&[ContentHash::from_bytes([7;32])]), |target| {
                unrelated.push(target);
                Ok(())
            }).expect("selected unrelated Thread");
        assert!(unrelated.is_empty(), "Thread selector constrains target discovery in SQL");
        let admitted = vec![super::super::collaboration_search::AdmittedSourceTarget {
            thread, revision, denied_leaves: vec![ContentHash::from_bytes([9;32])],
        }];
        let page = search_native_admitted(&directory, "alpha", None, 1, &[3], None,
            SourceSelection::Retained, &admitted).expect("first page");
        assert_eq!(page.scanned, 1);
        assert_eq!(page.hits[0].path, "visible.rs");
        let next = search_native_admitted(&directory, "alpha", Some(page.hits[0].cursor), 1,
            &[3], None, SourceSelection::Retained, &admitted).expect("next page");
        assert!(next.hits.is_empty());
        let hidden_only = search_native_admitted(&directory, "alpha", None, 1, &[3], None,
            SourceSelection::Retained, &[]).expect("hidden-only page");
        assert!(hidden_only.hits.is_empty());
        assert_eq!(hidden_only.scanned, 0);
        connection.execute("DELETE FROM source_search_candidate_leaves WHERE candidate=(SELECT candidate FROM source_search_candidates WHERE path='visible.rs')", [])
            .expect("drop one indexed proof leaf");
        assert!(!reader.readiness_for_authorized_target(thread, revision)
            .expect("incomplete leaf readiness").content,
            "a broken leaf chain must make the authorized source pending");
        let incomplete = search_native_admitted(&directory, "alpha", None, 1, &[3], None,
            SourceSelection::Retained, &admitted).expect("incomplete proof query");
        assert!(incomplete.hits.is_empty(), "missing leaf index cannot admit a path");
        connection.execute("DELETE FROM source_search_candidate_proofs WHERE candidate=(SELECT candidate FROM source_search_candidates WHERE path='hidden.rs')", [])
            .expect("drop path proof marker");
        let no_marker = search_native_admitted(&directory, "alpha", None, 1, &[3], None,
            SourceSelection::Retained, &[AdmittedSourceTarget { thread, revision, denied_leaves: Vec::new() }])
            .expect("missing proof marker query");
        assert!(no_marker.hits.is_empty(), "missing path proof cannot enter a page");
    }

    #[test]
    fn accepted_originals_queue_once_and_publish_or_defer_with_bounded_progress() {
        let root = tempfile::tempdir().expect("temporary metadata");
        let directory = root.path().join(".heddle");
        std::fs::create_dir(&directory).expect("metadata directory");
        let connection = crate::local_metadata::open(&directory).expect("metadata");
        super::super::initialize_schema(&connection).expect("source schema");
        let thread = ContentHash::from_bytes([1; 32]);
        let revision = StateId::from_bytes([2; 32]);
        let operation = ContentHash::from_bytes([3; 32]);
        connection.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),0,?3)",params![operation.as_bytes(),thread.as_bytes(),revision.as_bytes()]).expect("pending");
        assert!(due(&directory, 10, 1).expect("due").is_empty());
        connection
            .execute(
                "UPDATE operations SET status=1 WHERE id=?1",
                [operation.as_bytes().as_slice()],
            )
            .expect("admit");
        let reader = SourceSearchReader::open(&directory).expect("accepted target reader");
        let mut accepted = Vec::new();
        reader.visit_indexed_targets(super::super::collaboration_search::SourceSelection::Retained,
            Some(&[thread]), |target| { accepted.push(target); Ok(()) })
            .expect("unindexed accepted target");
        assert_eq!(accepted, vec![IndexedSourceTarget { thread, revision }]);
        assert_eq!(reader.readiness_for_authorized_target(thread, revision).expect("unindexed readiness"),
            SourceIndexReadiness { content: false, symbols: false });
        assert_eq!(
            due(&directory, 10, 1).expect("due"),
            vec![QueuedOriginal {
                operation,
                thread,
                revision
            }]
        );
        defer(&directory, operation, 20).expect("defer");
        assert!(due(&directory, 19, 1).expect("not due").is_empty());
        assert_eq!(
            due(&directory, 20, 1).expect("due"),
            vec![QueuedOriginal {
                operation,
                thread,
                revision
            }]
        );
        publish(&directory, thread, operation, revision, &[], true, false)
            .expect("content indexed");
        assert_eq!(reader.readiness_for_authorized_target(thread, revision).expect("zero-document readiness"),
            SourceIndexReadiness { content: true, symbols: false });
        assert!(due(&directory, 21, 1).expect("queue drained").is_empty());
        assert_eq!(next_due(&directory).expect("next"), None);
        assert!(due(&directory, 21, 33).is_err());
        connection.execute("UPDATE source_search_ready SET extractor_version=1 WHERE operation=?1",
            [operation.as_bytes().as_slice()]).expect("old path-only projection");
        connection.execute_batch(SCHEMA).expect("upgrade source projection");
        assert_eq!(due(&directory, 21, 1).expect("old projection requeued").len(), 1);
        publish(&directory, thread, operation, revision, &[], true, false)
            .expect("leaf-aware projection rebuilt");
        assert!(due(&directory, 21, 1).expect("upgraded queue drained").is_empty());
        connection
            .execute("DELETE FROM source_search_ready", [])
            .expect("legacy projection absent");
        connection
            .execute("DELETE FROM source_search_bootstrap", [])
            .expect("legacy install");
        connection.execute_batch(SCHEMA).expect("one-time backfill");
        assert_eq!(
            due(&directory, 21, 1).expect("backfilled"),
            vec![QueuedOriginal {
                operation,
                thread,
                revision
            }]
        );
        connection
            .execute(
                "UPDATE operations SET status=2 WHERE id=?1",
                [operation.as_bytes().as_slice()],
            )
            .expect("retire operation");
        assert_eq!(next_due(&directory).expect("retired work skipped"), None);
        connection
            .execute("DELETE FROM source_search_queue", [])
            .expect("drain");
        connection
            .execute_batch(SCHEMA)
            .expect("repeat initialization");
        assert!(
            due(&directory, 21, 1)
                .expect("no repeated full scan")
                .is_empty()
        );
    }

    #[test]
    fn hidden_source_corpus_cannot_change_visible_hit_score() {
        let root = tempfile::tempdir().expect("temporary metadata");
        let directory = root.path().join(".heddle");
        std::fs::create_dir(&directory).expect("metadata directory");
        let connection = crate::local_metadata::open(&directory).expect("metadata");
        super::super::initialize_schema(&connection).expect("source schema");
        let thread = ContentHash::from_bytes([1; 32]);
        let revision = StateId::from_bytes([2; 32]);
        let hidden_revision = StateId::from_bytes([5; 32]);
        let visible_operation = ContentHash::from_bytes([3; 32]);
        let hidden_operation = ContentHash::from_bytes([4; 32]);
        connection.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),1,?3)",params![visible_operation.as_bytes(),thread.as_bytes(),revision.as_bytes()]).expect("visible original");
        let document = |path: &str, text: &str| Document {
            kind: 3,
            path: path.into(),
            symbol_id: String::new(),
            symbol_name: String::new(),
            start_line: None,
            end_line: None,
            text: text.into(),
            leaf_chain: Vec::new(),
        };
        publish(
            &directory,
            thread,
            visible_operation,
            revision,
            &[document("visible.rs", "alpha visible")],
            true,
            false,
        )
        .expect("visible projection");
        let first = super::super::collaboration_search::search_native(
            &directory,
            "alpha",
            None,
            4,
            &[3],
            None,
            super::super::collaboration_search::SourceSelection::Retained,
        )
        .expect("visible query");
        assert_eq!(first.hits.len(), 1);
        let visible_score = first.hits[0].score;
        connection
            .execute(
                "INSERT INTO parents(child,parent) VALUES(?1,?2)",
                params![hidden_operation.as_bytes(), visible_operation.as_bytes()],
            )
            .expect("source causal edge");
        connection.execute("INSERT INTO operations(id,thread,facet,canonical,signature,status,source_revision) VALUES(?1,?2,1,x'00',zeroblob(64),1,?3)",params![hidden_operation.as_bytes(),thread.as_bytes(),hidden_revision.as_bytes()]).expect("newer original");
        publish(
            &directory,
            thread,
            hidden_operation,
            hidden_revision,
            &[document("hidden.rs", "alpha hidden alpha alpha")],
            true,
            false,
        )
        .expect("other projection");
        let second = super::super::collaboration_search::search_native(
            &directory,
            "alpha",
            None,
            4,
            &[3],
            None,
            super::super::collaboration_search::SourceSelection::Retained,
        )
        .expect("combined query");
        let visible = second
            .hits
            .iter()
            .find(|hit| hit.operation == visible_operation)
            .expect("visible hit retained");
        assert_eq!(
            visible.score, visible_score,
            "unserved corpus must not change a visible hit's score"
        );
        let current = super::super::collaboration_search::search_native(
            &directory,
            "alpha",
            None,
            4,
            &[3],
            None,
            super::super::collaboration_search::SourceSelection::Current,
        )
        .expect("current source");
        assert_eq!(current.hits.len(), 1);
        assert_eq!(
            current.hits[0].revision,
            Some(hidden_revision),
            "older accepted source does not fall back into current scope"
        );
        let exact = super::super::collaboration_search::search_native(
            &directory,
            "alpha",
            None,
            4,
            &[3],
            None,
            super::super::collaboration_search::SourceSelection::Exact(revision),
        )
        .expect("historical exact source");
        assert_eq!(exact.hits.len(), 1);
        assert_eq!(exact.hits[0].revision, Some(revision));
        connection
            .execute(
                "UPDATE source_search_ready SET extractor_version=4 WHERE operation=?1",
                [visible_operation.as_bytes().as_slice()],
            )
            .expect("stale extractor projection");
        let stale = super::super::collaboration_search::search_native(
            &directory,
            "alpha",
            None,
            4,
            &[3],
            None,
            super::super::collaboration_search::SourceSelection::Retained,
        )
        .expect("versioned source query");
        assert!(
            stale
                .hits
                .iter()
                .all(|hit| hit.operation != visible_operation),
            "stale extractor rows must not be served"
        );
        connection
            .execute_batch(SCHEMA)
            .expect("queue stale projection");
        assert!(
            due(&directory, chrono::Utc::now().timestamp(), 4)
                .expect("stale due")
                .iter()
                .any(|item| item.operation == visible_operation)
        );
    }
}
