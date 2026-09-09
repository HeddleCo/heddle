//! Mutable local state for one shared object store. Immutable source objects
//! stay in the object store; checkout filesystem changes retain their journals.
use std::path::Path;

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};

pub const DATABASE_NAME: &str = "metadata.sqlite3";
pub const CHANGE_MARKER_NAME: &str = "metadata.sqlite3.changed";
pub const SCHEMA_VERSION: i64 = 1;
pub const CHANGE_WINDOW: i64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Local metadata filesystem: {0}")]
    Io(#[from] std::io::Error),
    #[error("Local metadata database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("unsupported local metadata schema {0}")]
    Schema(i64),
    #[error("local metadata requires WAL mode; database selected {0}")]
    WalUnavailable(String),
    #[error("change cursor has expired; reload the authoritative snapshot")]
    CursorExpired,
    #[error("invalid local change cursor or page limit")]
    InvalidCursor,
}

/// The repository resolves worktree pointers before supplying its shared
/// heddle_dir. Schema creation commits atomically and never runs on hook reads.
pub fn open(heddle_dir: &Path) -> Result<Connection, Error> {
    let path = heddle_dir.join(DATABASE_NAME);
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
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    configure(&connection)?;
    let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    if mode != "wal" {
        return Err(Error::WalUnavailable(mode));
    }
    connection.execute_batch("PRAGMA synchronous=FULL;")?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(connection);
    }
    if version != 0 {
        return Err(Error::Schema(version));
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => {
            crate::thread_replication::initialize_schema(&tx)?;
            crate::device_runs::initialize_schema(&tx)?;
            crate::device_artifacts::initialize_schema(&tx)?;
            initialize_changes(&tx)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        SCHEMA_VERSION => {}
        other => return Err(Error::Schema(other)),
    }
    tx.commit()?;
    Ok(connection)
}

/// Opening an unknown store is side-effect free. Every initialized database
/// contains the complete core schema, regardless of which subsystem opened it.
pub fn open_existing(heddle_dir: &Path) -> Result<Option<Connection>, Error> {
    let path = heddle_dir.join(DATABASE_NAME);
    if !path.try_exists()? {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    configure(&connection)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(Error::Schema(version));
    }
    Ok(Some(connection))
}
fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(std::time::Duration::from_secs(5))
}

fn initialize_changes(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE metadata_change_state (singleton INTEGER PRIMARY KEY CHECK(singleton=1),floor INTEGER NOT NULL DEFAULT 0);
      INSERT INTO metadata_change_state(singleton) VALUES(1);
      CREATE TABLE metadata_changes(cursor INTEGER PRIMARY KEY AUTOINCREMENT,topic TEXT NOT NULL,entity TEXT NOT NULL);
      CREATE TRIGGER metadata_changes_bound AFTER INSERT ON metadata_changes BEGIN
        DELETE FROM metadata_changes WHERE cursor<=NEW.cursor-4096;
        UPDATE metadata_change_state SET floor=MAX(floor,NEW.cursor-4096) WHERE singleton=1;
      END;")?;
    // These are projections and wake cursors, not a second authored event log.
    // Triggers run inside the original mutation/receipt transaction; rollback
    // removes every associated wake. An OS marker only prompts a durable read.
    for (table, key, topic) in [
        ("threads", "hex(NEW.id)", "thread"),
        ("runs", "NEW.id", "run"),
        ("run_timeline", "NEW.run", "run"),
        ("run_controls", "NEW.run", "run"),
        ("run_permissions", "NEW.run", "run"),
        ("run_policies", "NEW.spool", "run_policy"),
        ("run_artifacts", "NEW.run", "run"),
    ] {
        // All identifiers and expressions are compile-time literals above.
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let key = if event == "DELETE" {
                key.replace("NEW.", "OLD.")
            } else {
                key.to_owned()
            };
            connection.execute_batch(&format!(
                "CREATE TRIGGER metadata_change_{table}_{event} AFTER {event} ON {table} BEGIN
                 INSERT INTO metadata_changes(topic,entity) VALUES('{topic}',{key}); END;"
            ))?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub cursor: i64,
    pub topic: String,
    pub entity: String,
}
#[derive(Debug)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    pub cursor: i64,
    pub exhausted: bool,
}
/// Return only committed changes, releasing the snapshot before the caller
/// writes any Iroh frame. A lagging subscriber must reset, never silently skip.
pub fn changes(connection: &mut Connection, after: i64, limit: usize) -> Result<ChangePage, Error> {
    if after < 0 || limit == 0 || limit > 1024 {
        return Err(Error::InvalidCursor);
    }
    let tx = connection.transaction()?;
    let (floor, high): (i64, i64) = tx.query_row(
        "SELECT floor,COALESCE((SELECT MAX(cursor) FROM metadata_changes),floor) FROM metadata_change_state WHERE singleton=1",
        [], |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    if after < floor {
        return Err(Error::CursorExpired);
    }
    if after > high {
        return Err(Error::InvalidCursor);
    }
    let mut changes = {
        let mut query = tx.prepare("SELECT cursor,topic,entity FROM metadata_changes WHERE cursor>?1 ORDER BY cursor LIMIT ?2")?;
        query
            .query_map(params![after, limit as i64 + 1], |row| {
                Ok(Change {
                    cursor: row.get(0)?,
                    topic: row.get(1)?,
                    entity: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let exhausted = changes.len() <= limit;
    changes.truncate(limit);
    let cursor = changes.last().map_or(after, |change| change.cursor);
    tx.commit()?;
    Ok(ChangePage {
        changes,
        cursor,
        exhausted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_state_receipt_and_cursor_commit_or_rollback_together() {
        let directory = tempfile::tempdir().expect("store");
        let mut writer = open(directory.path()).expect("schema");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(directory.path().join(DATABASE_NAME))
                .expect("database metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "private local metadata is not group/world-readable"
            );
        }
        let mut reader = open_existing(directory.path())
            .expect("existing")
            .expect("store");
        for commit in [false, true] {
            let tx = writer
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("write");
            let _concurrent_reader =
                open(directory.path()).expect("existing schema does not acquire writer lock");
            tx.execute("INSERT INTO threads(id,genesis,genesis_signature) VALUES(zeroblob(32),x'01',zeroblob(64))", []).expect("Thread state in same transaction");
            tx.execute(
                "INSERT INTO runs(id,thread,record) VALUES('run','thread',x'01')",
                [],
            )
            .expect("run");
            tx.execute("INSERT INTO run_commands(id,method,body,principal) VALUES('command','control',x'01','human')", []).expect("receipt");
            assert!(
                changes(&mut reader, 0, 10)
                    .expect("WAL reader during writer")
                    .changes
                    .is_empty()
            );
            if commit {
                tx.commit().expect("commit");
            } else {
                tx.rollback().expect("rollback");
            }
            let page = changes(&mut reader, 0, 10).expect("committed feed");
            assert_eq!(page.changes.len(), 2 * usize::from(commit));
            let receipts: i64 = reader
                .query_row("SELECT COUNT(*) FROM run_commands", [], |r| r.get(0))
                .expect("receipts");
            assert_eq!(receipts, i64::from(commit));
        }
        drop(writer);
        drop(reader);
        let mut reopened = open_existing(directory.path())
            .expect("reopen")
            .expect("store");
        assert_eq!(
            changes(&mut reopened, 0, 10)
                .expect("durable cursor")
                .changes
                .len(),
            2
        );
    }

    #[test]
    fn bounded_change_window_requires_reset_and_rejects_future_schema() {
        let directory = tempfile::tempdir().expect("store");
        let mut db = open(directory.path()).expect("schema");
        let tx = db.transaction().expect("write");
        for id in 0..CHANGE_WINDOW + 2 {
            tx.execute(
                "INSERT INTO runs(id,thread,record) VALUES(?1,'thread',x'01')",
                [id.to_string()],
            )
            .expect("run");
        }
        tx.commit().expect("commit");
        assert!(matches!(changes(&mut db, 0, 10), Err(Error::CursorExpired)));
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM metadata_changes", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, CHANGE_WINDOW);
        let page = changes(&mut db, 2, 10).expect("retained page");
        assert_eq!(page.changes.len(), 10);
        assert!(!page.exhausted);
        assert_eq!(page.cursor, 12);
        db.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("future schema");
        assert!(matches!(open(directory.path()), Err(Error::Schema(2))));
    }
}
