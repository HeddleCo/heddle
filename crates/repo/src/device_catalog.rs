//! Same-user local discovery of native Spools. Network input never supplies a
//! filesystem path to lookup; daemon requests resolve stable Spool UUIDs here.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSpool {
    pub id: uuid::Uuid,
    pub root: PathBuf,
    pub heddle_dir: PathBuf,
    /// Canonical capability resource path, set by locally authenticated setup.
    pub capability_path: String,
}
fn directory(home: &Path) -> PathBuf {
    home.join("state/device-rpc/spools")
}
pub fn register(home: &Path, repository: &crate::Repository, id: uuid::Uuid) -> Result<()> {
    if id.is_nil() {
        bail!("nil local spool identity");
    }
    let directory = directory(home);
    objects::fs_atomic::create_dir_all_durable(&directory)?;
    let file = directory.join(format!("{id}.json"));
    let _guard = objects::lock::RepoLock::at(directory.join(format!("{id}.lock"))).write()?;
    if file.exists() {
        let previous = load(home, id)?;
        if previous.root.exists() {
            return Ok(());
        }
    }
    let entry = DeviceSpool {
        id,
        root: repository.root().canonicalize()?,
        heddle_dir: repository.heddle_dir().canonicalize()?,
        capability_path: id.to_string(),
    };
    objects::fs_atomic::write_file_atomic_secret(&file, &serde_json::to_vec(&entry)?)?;
    Ok(())
}
pub fn load(home: &Path, id: uuid::Uuid) -> Result<DeviceSpool> {
    if id.is_nil() {
        bail!("nil local spool identity");
    }
    let file = directory(home).join(format!("{id}.json"));
    if std::fs::metadata(&file)?.len() > 16 * 1024 {
        bail!("local spool registration exceeds bound");
    }
    let entry: DeviceSpool = serde_json::from_slice(&bounded_read(&file, 16 * 1024)?)?;
    if entry.id != id || entry.capability_path.is_empty() {
        bail!("local spool registration differs from lookup");
    }
    let actual = std::fs::read_to_string(entry.heddle_dir.join("spool-id"))?;
    if actual.trim() != id.to_string() {
        bail!("registered repository changed spool identity");
    }
    Ok(entry)
}
/// Called by authenticated local setup after resolving a hosted path. This is
/// presentation/scope binding; it never changes the stable Spool identifier.
pub fn set_capability_path(home: &Path, id: uuid::Uuid, path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("invalid spool capability path");
    }
    let directory = directory(home);
    let _guard = objects::lock::RepoLock::at(directory.join(format!("{id}.lock"))).write()?;
    let mut entry = load(home, id)?;
    entry.capability_path = path.to_owned();
    objects::fs_atomic::write_file_atomic_secret(
        &directory.join(format!("{id}.json")),
        &serde_json::to_vec(&entry)?,
    )?;
    Ok(())
}
/// Resolve a checkout only from locally registered bindings, never a caller path.
pub fn checkout(
    spool: &DeviceSpool,
    id: &str,
) -> Result<crate::thread_replication::checkout::ThreadCheckout> {
    let id = uuid::Uuid::parse_str(id).context("checkout id must be a UUID")?;
    let file = spool
        .heddle_dir
        .join("native-checkouts")
        .join(format!("{id}.json"));
    if std::fs::metadata(&file)?.len() > 16 * 1024 {
        bail!("checkout registration exceeds bound");
    }
    let path: PathBuf = serde_json::from_slice(&bounded_read(&file, 16 * 1024)?)?;
    let checkout = crate::thread_replication::checkout::ThreadCheckout::open(&path)?;
    if checkout.binding.id != id.to_string()
        || checkout.repository.heddle_dir().canonicalize()? != spool.heddle_dir
    {
        bail!("checkout registration changed");
    }
    Ok(checkout)
}

/// Persist replay protection across daemon restarts. The timestamp window is
/// checked cryptographically before this function; expired entries are bounded.
pub fn claim_nonce(home: &Path, identity: &str, nonce: &[u8], now: i64) -> Result<bool> {
    let directory = home.join("state/device-rpc");
    objects::fs_atomic::create_private_dir_all(&directory)?;
    let mut connection = rusqlite::Connection::open(directory.join("nonces.sqlite3"))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS nonces(identity TEXT NOT NULL,nonce BLOB NOT NULL,at INTEGER NOT NULL,PRIMARY KEY(identity,nonce)); CREATE INDEX IF NOT EXISTS nonce_expiry ON nonces(at);")?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM nonces WHERE at<?1",
        [now.saturating_sub(120_000)],
    )?;
    let fresh = tx.execute(
        "INSERT OR IGNORE INTO nonces(identity,nonce,at) VALUES (?1,?2,?3)",
        rusqlite::params![identity, nonce, now],
    )? == 1;
    tx.commit()?;
    Ok(fresh)
}

fn bounded_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("device registration exceeds read budget");
    }
    Ok(bytes)
}
