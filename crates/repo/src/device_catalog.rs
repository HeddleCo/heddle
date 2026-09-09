//! Same-user local discovery of native Spools. Network input never supplies a
//! filesystem path to lookup; daemon requests resolve stable Spool UUIDs here.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub mod mutations;
pub mod store;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSpool {
    pub id: uuid::Uuid,
    pub root: PathBuf,
    pub heddle_dir: PathBuf,
    /// Canonical capability resource path, set by locally authenticated setup.
    pub capability_path: String,
}
pub fn register(home: &Path, repository: &crate::Repository, id: uuid::Uuid) -> Result<()> {
    if id.is_nil() {
        bail!("nil local spool identity");
    }
    let entry = DeviceSpool {
        id,
        root: repository.root().canonicalize()?,
        heddle_dir: repository.heddle_dir().canonicalize()?,
        capability_path: id.to_string(),
    };
    let overview = api::heddle::api::v2alpha1::SpoolOverview {
        name: repository
            .root()
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Local Spool")
            .to_owned(),
        slug: id.to_string(),
        audience: api::heddle::api::v2alpha1::Audience::Private as i32,
        settings: Some(api::heddle::api::v2alpha1::SpoolSettings {
            audience: api::heddle::api::v2alpha1::Audience::Private as i32,
            default_state_audience: api::heddle::api::v2alpha1::Audience::Private as i32,
            allow_child_creation: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    store::Catalog::open(home)?.register(&entry, overview)
}
pub fn load(home: &Path, id: uuid::Uuid) -> Result<DeviceSpool> {
    if id.is_nil() {
        bail!("nil local spool identity");
    }
    let entry = store::Catalog::read(home)?
        .context("local catalog unavailable")?
        .spool(id)?
        .context("local Spool unavailable")?
        .registration;
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
    // Revalidate the physical binding before changing its capability path.
    load(home, id)?;
    store::Catalog::open(home)?.set_capability_path(id, path)
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
