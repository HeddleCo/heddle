// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use objects::object::{ContentHash, derived::*};
use rusqlite::{Connection, OptionalExtension, params};

use super::storage_error;

// Local rows are bounded independently of an executor's working budget.
const MAX_LOCAL_NODE_BYTES: usize = 1024 * 1024;

fn read_blob(
    connection: &Connection,
    hash: ContentHash,
    max_bytes: usize,
) -> DerivedResult<Option<Vec<u8>>> {
    let cap = max_bytes.min(MAX_LOCAL_NODE_BYTES);
    let mut statement = connection.prepare_cached(
        "SELECT length(bytes), CASE WHEN length(bytes)<=?2 THEN bytes END FROM derived_nodes WHERE hash=?1"
    ).map_err(storage_error)?;
    let mut rows = statement
        .query(params![hash.as_bytes().as_slice(), cap as u32])
        .map_err(storage_error)?;
    let Some(row) = rows.next().map_err(storage_error)? else {
        return Ok(None);
    };
    let len: i64 = row.get(0).map_err(storage_error)?;
    if len < 0 || len as u64 > cap as u64 {
        return Err(DerivedError::Limit("local node bytes"));
    }
    let bytes = row
        .get_ref(1)
        .map_err(storage_error)?
        .as_blob()
        .map_err(storage_error)?;
    if node_hash(bytes) != hash {
        return Err(DerivedError::Corrupt("node hash"));
    }
    Ok(Some(bytes.to_vec()))
}

/// Local immutable results. The caller chooses a path under repository state.
/// SQLite transactions publish nodes before bindings; concurrent writers either
/// agree byte-for-byte or get a conflict. Nodes are capped at 1 MiB; the page
/// cache is capped independently at 1 MiB. No transaction spans source/host I/O.
pub struct LocalDerivedStore {
    connection: Connection,
}

impl LocalDerivedStore {
    pub fn open(path: impl AsRef<Path>) -> DerivedResult<Self> {
        let connection = Connection::open(path).map_err(storage_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             PRAGMA cache_size=-1024;
             PRAGMA mmap_size=0;
             CREATE TABLE IF NOT EXISTS derived_nodes (
                 hash BLOB PRIMARY KEY CHECK(length(hash)=32), bytes BLOB NOT NULL
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS derived_bindings (
                 key BLOB PRIMARY KEY CHECK(length(key)=68),
                 root BLOB NOT NULL REFERENCES derived_nodes(hash)
             ) WITHOUT ROWID;",
            )
            .map_err(storage_error)?;
        Ok(Self { connection })
    }
}

impl DerivedStore for LocalDerivedStore {
    fn lookup(&self, key: DerivedKey) -> DerivedResult<Option<ContentHash>> {
        self.connection
            .query_row(
                "SELECT root FROM derived_bindings WHERE key=?1",
                [key.encode().as_slice()],
                |row| row.get::<_, [u8; 32]>(0),
            )
            .optional()
            .map(|v| v.map(ContentHash::from_bytes))
            .map_err(storage_error)
    }

    fn get_node(&self, hash: ContentHash, max_bytes: usize) -> DerivedResult<Option<Vec<u8>>> {
        read_blob(&self.connection, hash, max_bytes)
    }

    fn put_node_if_absent(&self, hash: ContentHash, bytes: &[u8]) -> DerivedResult<()> {
        if bytes.len() > MAX_LOCAL_NODE_BYTES {
            return Err(DerivedError::Limit("local node bytes"));
        }
        if node_hash(bytes) != hash {
            return Err(DerivedError::Corrupt("node hash"));
        }
        let node = DerivedNode::decode(bytes)?;
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(storage_error)?;
        // Dependencies must already exist, with the exact identity embedded in
        // the parent handle. Partial work can leave nodes, never ready bindings.
        for parent in &node.parents {
            let parent_bytes = read_blob(&tx, parent.root, MAX_LOCAL_NODE_BYTES)?
                .ok_or(DerivedError::MissingValue(parent.root))?;
            DerivedNode::decode(&parent_bytes)?.check_key(parent.key)?;
        }
        tx.execute(
            "INSERT INTO derived_nodes(hash,bytes) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![hash.as_bytes().as_slice(), bytes],
        )
        .map_err(storage_error)?;
        let same: bool = tx
            .query_row(
                "SELECT bytes=?2 FROM derived_nodes WHERE hash=?1",
                params![hash.as_bytes().as_slice(), bytes],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if !same {
            return Err(DerivedError::Conflict);
        }
        tx.commit().map_err(storage_error)
    }

    fn bind_if_absent(&self, key: DerivedKey, root: ContentHash) -> DerivedResult<()> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(storage_error)?;
        let bytes =
            read_blob(&tx, root, MAX_LOCAL_NODE_BYTES)?.ok_or(DerivedError::MissingValue(root))?;
        // Treat publishing another root at the same key as a conflict even if
        // the replacement also has an invalid envelope.
        let old: Option<[u8; 32]> = tx
            .query_row(
                "SELECT root FROM derived_bindings WHERE key=?1",
                [key.encode().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if old.is_some_and(|old| old != *root.as_bytes()) {
            return Err(DerivedError::Conflict);
        }
        DerivedNode::decode(&bytes)?.check_key(key)?;
        tx.execute(
            "INSERT INTO derived_bindings(key,root) VALUES (?1,?2) ON CONFLICT DO NOTHING",
            params![key.encode().as_slice(), root.as_bytes().as_slice()],
        )
        .map_err(storage_error)?;
        tx.commit().map_err(storage_error)
    }
}
