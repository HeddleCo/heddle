//! One device-local transaction boundary for discovery, settings and navigation.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use api::heddle::api::v2alpha1 as wire;
use prost::Message;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use super::DeviceSpool;

pub const MAX_SPOOLS: usize = 4096;
pub const MAX_RECORD_BYTES: usize = 256 * 1024;
pub const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const SCHEMA: &str = "
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS catalog_schema(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
INSERT OR IGNORE INTO catalog_schema VALUES(1,1);
CREATE TABLE IF NOT EXISTS catalog_generation(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
INSERT OR IGNORE INTO catalog_generation VALUES(1,0);
CREATE TABLE IF NOT EXISTS spools(id TEXT PRIMARY KEY,registration BLOB NOT NULL,overview BLOB NOT NULL,capability_path TEXT NOT NULL,version INTEGER NOT NULL CHECK(version>0),deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)),parent TEXT NOT NULL DEFAULT '',slug TEXT NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS live_spool_names ON spools(parent,slug) WHERE deleted=0;
CREATE UNIQUE INDEX IF NOT EXISTS live_spool_paths ON spools(capability_path) WHERE deleted=0;
CREATE TABLE IF NOT EXISTS mounts(id TEXT PRIMARY KEY,parent TEXT NOT NULL REFERENCES spools(id),child TEXT NOT NULL REFERENCES spools(id),name TEXT NOT NULL,version INTEGER NOT NULL CHECK(version>0),deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN(0,1)));
CREATE UNIQUE INDEX IF NOT EXISTS live_mount_names ON mounts(parent,name) WHERE deleted=0;
CREATE INDEX IF NOT EXISTS mounts_child ON mounts(child) WHERE deleted=0;
CREATE TABLE IF NOT EXISTS bookmarks(account TEXT NOT NULL,target BLOB NOT NULL,record BLOB NOT NULL,version INTEGER NOT NULL CHECK(version>0),PRIMARY KEY(account,target));
CREATE TABLE IF NOT EXISTS receipts(account TEXT NOT NULL,method TEXT NOT NULL,operation TEXT NOT NULL,request_hash BLOB NOT NULL,response BLOB NOT NULL,PRIMARY KEY(account,method,operation));
CREATE TRIGGER IF NOT EXISTS spool_catalog_generation_insert AFTER INSERT ON spools BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS spool_catalog_generation_update AFTER UPDATE ON spools BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS mount_catalog_generation_insert AFTER INSERT ON mounts BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS mount_catalog_generation_update AFTER UPDATE ON mounts BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS bookmark_catalog_generation_insert AFTER INSERT ON bookmarks BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS bookmark_catalog_generation_update AFTER UPDATE ON bookmarks BEGIN UPDATE catalog_generation SET version=version+1 WHERE id=1; END;
";
#[derive(Clone, Debug)]
pub struct SpoolRecord {
    pub registration: DeviceSpool,
    pub overview: wire::SpoolOverview,
}
#[derive(Debug)]
pub struct Catalog {
    connection: Connection,
}
#[derive(Debug)]
pub struct Page<T> {
    pub records: Vec<T>,
    pub has_more: bool,
}

pub fn database_path(home: &Path) -> PathBuf {
    home.join("state/device-rpc/catalog.sqlite3")
}
impl Catalog {
    /// Explicit local setup/mutation may create a store. Read paths use `read`.
    pub fn open(home: &Path) -> Result<Self> {
        let path = database_path(home);
        objects::fs_atomic::create_private_dir_all(path.parent().context("catalog parent")?)?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let initialized:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='catalog_schema')",[],|row|row.get(0))?;
        if initialized {
            return Self::checked(connection);
        }
        connection.pragma_update(None, "journal_mode", "WAL")?;
        let tx = connection.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.commit()?;
        Self::checked(connection)
    }
    pub fn read(home: &Path) -> Result<Option<Self>> {
        let path = database_path(home);
        if !path.try_exists()? {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::checked(connection).map(Some)
    }
    fn checked(connection: Connection) -> Result<Self> {
        let version: i64 =
            connection.query_row("SELECT version FROM catalog_schema WHERE id=1", [], |row| {
                row.get(0)
            })?;
        if version != 1 {
            bail!("unsupported device catalog schema")
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { connection })
    }
    fn announce(&self) -> Result<()> {
        let path = self
            .connection
            .path()
            .context("catalog database path missing")?;
        objects::fs_atomic::write_file_atomic_secret(
            &Path::new(path).with_extension("sqlite3.changed"),
            &self.generation()?.to_be_bytes(),
        )?;
        Ok(())
    }
    pub fn generation(&self) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT version FROM catalog_generation WHERE id=1",
            [],
            |row| row.get(0),
        )?)
    }
    pub fn registrations(&self) -> Result<Vec<DeviceSpool>> {
        let mut statement=self.connection.prepare("SELECT CASE WHEN length(registration)<=16384 THEN registration END FROM spools WHERE deleted=0 ORDER BY id LIMIT 4097")?;
        let mut rows = statement.query([])?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        while let Some(row) = rows.next()? {
            let encoded: Vec<u8> = row
                .get::<_, Option<Vec<u8>>>(0)?
                .context("stored registration exceeds bound")?;
            bytes = bytes
                .checked_add(encoded.len())
                .context("catalog byte overflow")?;
            if result.len() >= MAX_SPOOLS || bytes > MAX_PAGE_BYTES {
                bail!("device registration inventory exceeds bound");
            }
            result.push(serde_json::from_slice(&encoded)?);
        }
        Ok(result)
    }
    pub fn spool(&self, id: uuid::Uuid) -> Result<Option<SpoolRecord>> {
        spool_in(&self.connection, id)
    }
    pub fn spools(&self, after: &str, limit: usize, max_bytes: usize) -> Result<Page<SpoolRecord>> {
        if limit == 0 || limit > MAX_SPOOLS || max_bytes == 0 || max_bytes > MAX_PAGE_BYTES {
            bail!("invalid catalog page budget")
        }
        if !after.is_empty() {
            uuid::Uuid::parse_str(after)?;
        }
        let mut statement=self.connection.prepare("SELECT CASE WHEN length(registration)<=16384 THEN registration END,CASE WHEN length(overview)<=262144 THEN overview END,version,length(registration)+length(overview) FROM spools WHERE deleted=0 AND id>?1 ORDER BY id LIMIT ?2")?;
        let mut rows = statement.query(params![after, i64::try_from(limit + 1)?])?;
        let mut records = Vec::new();
        let mut bytes = 0usize;
        let mut more = false;
        while let Some(row) = rows.next()? {
            let size = usize::try_from(row.get::<_, i64>(3)?)?;
            if records.len() == limit || bytes.saturating_add(size) > max_bytes {
                if records.is_empty() {
                    bail!("catalog row exceeds page byte budget")
                }
                more = true;
                break;
            }
            let registration: Vec<u8> = row
                .get::<_, Option<Vec<u8>>>(0)?
                .context("registration exceeds stored byte bound")?;
            let overview: Vec<u8> = row
                .get::<_, Option<Vec<u8>>>(1)?
                .context("overview exceeds stored byte bound")?;
            records.push(decode(&registration, &overview, row.get(2)?)?);
            bytes += size;
        }
        Ok(Page {
            records,
            has_more: more,
        })
    }
    /// Registered paths are trusted local setup, never supplied by an RPC.
    pub fn register(
        &mut self,
        registration: &DeviceSpool,
        overview: wire::SpoolOverview,
    ) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(mut prior) = spool_in(&tx, registration.id)? {
            if prior.registration.root.try_exists()? {
                tx.rollback()?;
                self.announce()?;
                return Ok(());
            }
            // Re-discovery of a moved local checkout preserves its authenticated
            // path and settings; only its locally trusted physical binding moves.
            prior.registration.root = registration.root.clone();
            prior.registration.heddle_dir = registration.heddle_dir.clone();
            let bytes = serde_json::to_vec(&prior.registration)?;
            if bytes.len() > 16 * 1024 {
                bail!("local registration exceeds bound");
            }
            tx.execute(
                "UPDATE spools SET registration=?2,version=version+1 WHERE id=?1",
                params![registration.id.to_string(), bytes],
            )?;
            tx.commit()?;
            self.announce()?;
            return Ok(());
        }
        insert_spool_in(&tx, registration, &overview)?;
        tx.commit()?;
        self.announce()?;
        Ok(())
    }
    pub fn set_capability_path(&mut self, id: uuid::Uuid, path: &str) -> Result<()> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut record = spool_in(&tx, id)?.context("local Spool unavailable")?;
        record.registration.capability_path = path.to_owned();
        tx.execute("UPDATE spools SET capability_path=?2,registration=?3,version=version+1 WHERE id=?1 AND deleted=0",params![id.to_string(),path,serde_json::to_vec(&record.registration)?])?;
        tx.commit()?;
        self.announce()?;
        Ok(())
    }
    /// Receipts and records commit together. The caller holds current account
    /// authority through this closure and invokes its final time check inside it.
    pub fn mutate<M: Message + Default>(
        &mut self,
        account: &str,
        method: &str,
        operation: &str,
        request: &[u8],
        change: impl FnOnce(&Transaction<'_>) -> Result<M>,
    ) -> Result<M> {
        uuid::Uuid::parse_str(account)?;
        uuid::Uuid::parse_str(operation)?;
        if method.is_empty() || method.len() > 256 || request.len() > 1024 * 1024 {
            bail!("catalog mutation exceeds bound")
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let hash = blake3::hash(request);
        let existing:Option<(Vec<u8>,Vec<u8>)>=tx.query_row("SELECT request_hash,response FROM receipts WHERE account=?1 AND method=?2 AND operation=?3",params![account,method,operation],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((prior, response)) = existing {
            if prior.as_slice() != hash.as_bytes() {
                bail!("operation ID was reused with different content")
            }
            if response.len() > MAX_PAGE_BYTES {
                bail!("stored catalog receipt exceeds bound")
            }
            let result = M::decode(response.as_slice())?;
            tx.rollback()?;
            self.announce()?;
            return Ok(result);
        }
        let result = change(&tx)?;
        let bytes = result.encode_to_vec();
        if bytes.len() > MAX_PAGE_BYTES {
            bail!("catalog receipt exceeds bound")
        }
        tx.execute("INSERT INTO receipts(account,method,operation,request_hash,response) VALUES(?1,?2,?3,?4,?5)",params![account,method,operation,hash.as_bytes().as_slice(),bytes])?;
        tx.commit()?;
        self.announce()?;
        Ok(result)
    }
}
/// Create a catalog identity inside the same receipt transaction as its caller.
/// Network handlers construct the registration path locally, never from input.
pub fn insert_spool_in(
    tx: &Transaction<'_>,
    registration: &DeviceSpool,
    overview: &wire::SpoolOverview,
) -> Result<()> {
    if registration.id.is_nil()
        || registration.capability_path.is_empty()
        || registration.capability_path.len() > 4096
    {
        bail!("invalid local Spool registration");
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM spools WHERE deleted=0", [], |row| {
        row.get(0)
    })?;
    if usize::try_from(count)? >= MAX_SPOOLS {
        bail!("device catalog Spool capacity exceeded")
    }
    let registration_bytes = serde_json::to_vec(registration)?;
    if registration_bytes.len() > 16 * 1024 {
        bail!("local registration exceeds bound")
    }
    let mut overview = overview.clone();
    overview.r#ref = Some(wire::SpoolRef {
        id: registration.id.to_string(),
    });
    overview.version = 1i64.to_be_bytes().to_vec();
    bounded(&overview)?;
    let parent = overview
        .parent
        .as_ref()
        .map(|value| value.id.as_str())
        .unwrap_or("");
    tx.execute("INSERT INTO spools(id,registration,overview,capability_path,version,parent,slug) VALUES(?1,?2,?3,?4,1,?5,?6)",params![registration.id.to_string(),registration_bytes,overview.encode_to_vec(),registration.capability_path,parent,overview.slug])?;
    Ok(())
}
pub fn spool_in(connection: &Connection, id: uuid::Uuid) -> Result<Option<SpoolRecord>> {
    let row: Option<(Vec<u8>, Vec<u8>, i64)> = connection
        .query_row(
            "SELECT CASE WHEN length(registration)<=16384 THEN registration END,CASE WHEN length(overview)<=262144 THEN overview END,version FROM spools WHERE id=?1 AND deleted=0",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(registration, overview, version)| decode(&registration, &overview, version))
        .transpose()
}
fn decode(registration: &[u8], overview: &[u8], version: i64) -> Result<SpoolRecord> {
    if registration.len() > 16 * 1024 || overview.len() > MAX_RECORD_BYTES || version <= 0 {
        bail!("invalid catalog row bounds")
    }
    let registration: DeviceSpool = serde_json::from_slice(registration)?;
    let mut overview = wire::SpoolOverview::decode(overview)?;
    if overview
        .r#ref
        .as_ref()
        .is_none_or(|value| value.id != registration.id.to_string())
    {
        bail!("catalog stable identity mismatch")
    }
    overview.version = version.to_be_bytes().to_vec();
    Ok(SpoolRecord {
        registration,
        overview,
    })
}
pub fn bounded(value: &impl Message) -> Result<()> {
    if value.encoded_len() > MAX_RECORD_BYTES {
        bail!("catalog record exceeds 256 KiB")
    }
    Ok(())
}
pub fn exact_version(actual: &[u8], expected: &[u8]) -> Result<()> {
    if actual != expected {
        bail!("catalog version changed")
    }
    Ok(())
}
