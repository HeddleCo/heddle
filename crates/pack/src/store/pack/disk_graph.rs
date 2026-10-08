// SPDX-License-Identifier: Apache-2.0
//! Scratch membership and work queue. SQLite's page cache is fixed at 2 MiB;
//! neither the queue nor the visited identities are collected in Rust memory.
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use tempfile::NamedTempFile;

use super::{ObjectType, PackObjectId};
use crate::{
    object::ContentHash,
    store::{Result, StoreError},
};

pub(super) struct DiskGraph {
    connection: Connection,
    _file: NamedTempFile,
    count: usize,
}
fn sql(error: rusqlite::Error) -> StoreError {
    StoreError::InvalidObject(format!("source scratch database: {error}"))
}
impl DiskGraph {
    pub fn new(root: &Path) -> Result<Self> {
        let file = NamedTempFile::new_in(root)?;
        let connection = Connection::open(file.path()).map_err(sql)?;
        connection.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
            CREATE TABLE objects(id BLOB PRIMARY KEY, kind INTEGER NOT NULL, pending INTEGER NOT NULL) WITHOUT ROWID;
            CREATE INDEX pending_objects ON objects(id) WHERE pending=1;
            CREATE TABLE files(id BLOB PRIMARY KEY) WITHOUT ROWID;
            CREATE TABLE leaves(tree BLOB, hash BLOB, PRIMARY KEY(tree,hash)) WITHOUT ROWID;
            BEGIN;").map_err(sql)?;
        Ok(Self {
            connection,
            _file: file,
            count: 0,
        })
    }
    pub fn insert(&mut self, id: PackObjectId, kind: ObjectType) -> Result<bool> {
        let mut key = Vec::with_capacity(33);
        id.encode_tagged(&mut key);
        let changed = self
            .connection
            .prepare_cached("INSERT OR IGNORE INTO objects VALUES (?1, ?2, ?3)")
            .map_err(sql)?
            .execute(params![
                key,
                kind as u8,
                matches!(kind, ObjectType::Tree | ObjectType::State)
            ])
            .map_err(sql)?;
        if changed == 0 {
            let expected: u8 = self
                .connection
                .prepare_cached("SELECT kind FROM objects WHERE id=?1")
                .map_err(sql)?
                .query_row([&key], |row| row.get(0))
                .map_err(sql)?;
            if expected != kind as u8 {
                return Err(StoreError::InvalidObject(
                    "source object is referenced with conflicting types".into(),
                ));
            }
            return Ok(false);
        }
        self.count += 1;
        Ok(true)
    }
    pub fn pop(&mut self) -> Result<Option<(PackObjectId, ObjectType)>> {
        let row: Option<(Vec<u8>, u8)> = self
            .connection
            .prepare_cached(
                "SELECT id,kind FROM objects INDEXED BY pending_objects WHERE pending=1 LIMIT 1",
            )
            .map_err(sql)?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
            .map_err(sql)?;
        let Some((key, kind)) = row else {
            return Ok(None);
        };
        self.connection
            .prepare_cached("UPDATE objects SET pending=0 WHERE id=?1")
            .map_err(sql)?
            .execute([&key])
            .map_err(sql)?;
        let (id, _) = PackObjectId::decode_tagged(&key)?;
        let kind = ObjectType::from_u8(kind)
            .ok_or_else(|| StoreError::InvalidObject("scratch object type invalid".into()))?;
        Ok(Some((id, kind)))
    }
    pub fn contains(&self, id: PackObjectId) -> Result<bool> {
        let mut key = Vec::with_capacity(33);
        id.encode_tagged(&mut key);
        Ok(self
            .connection
            .prepare_cached("SELECT 1 FROM objects WHERE id=?1")
            .map_err(sql)?
            .query_row([key], |_| Ok(()))
            .optional()
            .map_err(sql)?
            .is_some())
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn insert_file(&mut self, id: ContentHash) -> Result<()> {
        self.connection
            .prepare_cached("INSERT OR IGNORE INTO files VALUES (?1)")
            .map_err(sql)?
            .execute([id.as_bytes()])
            .map_err(sql)?;
        Ok(())
    }
    pub fn contains_file(&self, id: ContentHash) -> Result<bool> {
        Ok(self
            .connection
            .prepare_cached("SELECT 1 FROM files WHERE id=?1")
            .map_err(sql)?
            .query_row([id.as_bytes()], |_| Ok(()))
            .optional()
            .map_err(sql)?
            .is_some())
    }
    pub fn insert_leaf(&mut self, tree: ContentHash, leaf: ContentHash) -> Result<()> {
        self.connection
            .prepare_cached("INSERT OR IGNORE INTO leaves VALUES (?1, ?2)")
            .map_err(sql)?
            .execute(params![tree.as_bytes(), leaf.as_bytes()])
            .map_err(sql)?;
        Ok(())
    }
    pub fn contains_leaf(&self, tree: ContentHash, leaf: ContentHash) -> Result<bool> {
        Ok(self
            .connection
            .prepare_cached("SELECT 1 FROM leaves WHERE tree=?1 AND hash=?2")
            .map_err(sql)?
            .query_row(params![tree.as_bytes(), leaf.as_bytes()], |_| Ok(()))
            .optional()
            .map_err(sql)?
            .is_some())
    }
}
