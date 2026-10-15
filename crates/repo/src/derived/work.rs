// SPDX-License-Identifier: Apache-2.0
//! Disk-backed worklists keep history depth out of executor-owned heap memory.
use objects::object::{ContentHash, derived::*};
use rusqlite::{Connection, OptionalExtension, params};

use super::Limits;

fn storage_error(error: rusqlite::Error) -> DerivedError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error
        && failure.code == rusqlite::ErrorCode::DiskFull
    {
        return DerivedError::Limit("scratch bytes");
    }
    DerivedError::Storage(error.to_string())
}

pub(super) struct Work {
    pub connection: Connection,
    // Drop the connection before its file, including on error.
    _file: tempfile::NamedTempFile,
}

impl Work {
    pub fn new(limits: Limits) -> DerivedResult<Self> {
        let file = tempfile::NamedTempFile::new().map_err(super::storage_error)?;
        let connection = Connection::open(file.path()).map_err(storage_error)?;
        if limits.max_scratch_bytes < 16 * 4096 {
            return Err(DerivedError::Limit("scratch bytes"));
        }
        connection.execute_batch("PRAGMA page_size=4096; PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;
            CREATE TABLE frames (depth INTEGER PRIMARY KEY, state BLOB UNIQUE NOT NULL, input BLOB NOT NULL, next_parent INTEGER NOT NULL);
            CREATE TABLE visits (hash BLOB PRIMARY KEY, state BLOB, done INTEGER NOT NULL) WITHOUT ROWID; CREATE INDEX pending_visits ON visits(hash) WHERE done=0;").map_err(storage_error)?;
        connection
            .pragma_update(
                None,
                "max_page_count",
                u32::try_from(limits.max_scratch_bytes / 4096)
                    .map_err(|_| DerivedError::Limit("scratch pages"))?,
            )
            .map_err(storage_error)?;
        Ok(Self {
            connection,
            _file: file,
        })
    }

    pub fn push(&self, depth: u32, input: &DerivedInput) -> DerivedResult<()> {
        let active: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM frames WHERE state=?1)",
                [input.state.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        if active {
            return Err(DerivedError::Cycle(input.state));
        }
        self.connection
            .execute(
                "INSERT INTO frames VALUES (?1,?2,?3,0)",
                params![depth, input.state.as_bytes().as_slice(), input.encode()?],
            )
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn top(&self, depth: u32) -> DerivedResult<(DerivedInput, usize)> {
        self.connection
            .query_row(
                "SELECT input,next_parent FROM frames WHERE depth=?1",
                [depth],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u32>(1)? as usize)),
            )
            .map_err(storage_error)
            .and_then(|(bytes, next)| Ok((DerivedInput::decode(&bytes)?, next)))
    }

    pub fn advance(&self, depth: u32) -> DerivedResult<()> {
        self.connection
            .execute(
                "UPDATE frames SET next_parent=next_parent+1 WHERE depth=?1",
                [depth],
            )
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn pop(&self, depth: u32) -> DerivedResult<()> {
        self.connection
            .execute("DELETE FROM frames WHERE depth=?1", [depth])
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn visit(&self, hash: ContentHash, key: Option<DerivedKey>) -> DerivedResult<()> {
        let old: Option<Option<[u8; 32]>> = self
            .connection
            .query_row(
                "SELECT state FROM visits WHERE hash=?1",
                [hash.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if let (Some(Some(old)), Some(key)) = (old, key)
            && old != *key.state.as_bytes()
        {
            return Err(DerivedError::Corrupt("conflicting dependency identity"));
        }
        self.connection
            .execute(
                "INSERT INTO visits VALUES (?1,?2,0) ON CONFLICT DO NOTHING",
                params![
                    hash.as_bytes().as_slice(),
                    key.map(|k| k.state.as_bytes().to_vec())
                ],
            )
            .map_err(storage_error)?;
        Ok(())
    }

    pub fn next_visit(&self) -> DerivedResult<Option<(ContentHash, Option<DerivedKey>)>> {
        let hash = self
            .connection
            .query_row(
                "SELECT hash,state FROM visits WHERE done=0 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, [u8; 32]>(0)?,
                        row.get::<_, Option<[u8; 32]>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?;
        if let Some((hash, _)) = hash {
            self.connection
                .execute("UPDATE visits SET done=1 WHERE hash=?1", [hash.as_slice()])
                .map_err(storage_error)?;
        }
        Ok(hash.map(|(hash, state)| {
            (
                ContentHash::from_bytes(hash),
                state.map(|s| {
                    StateClosure
                        .descriptor()
                        .key(objects::object::StateId::from_bytes(s))
                }),
            )
        }))
    }

    pub fn bytes(&self) -> DerivedResult<u64> {
        let pages: u32 = self
            .connection
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .map_err(storage_error)?;
        Ok(u64::from(pages) * 4096)
    }
}
