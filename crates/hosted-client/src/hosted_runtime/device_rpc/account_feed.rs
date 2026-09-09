//! One OS-fed account watcher shared by all account subscriptions. Idle streams
//! do not reopen the catalog, scan repositories or execute SQLite queries.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Result, bail};
use repo::{device_catalog::store::Catalog, device_watch::DeviceWatch};
use tokio::sync::watch;

pub(super) struct AccountFeed {
    pub changes: watch::Sender<u64>,
    watcher: Mutex<DeviceWatch>,
    watched: Mutex<BTreeSet<PathBuf>>,
    // Keep WAL bookkeeping alive so reads never create their own change cycle.
    catalog: Mutex<Option<Catalog>>,
    home: PathBuf,
}
impl std::fmt::Debug for AccountFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountFeed").finish_non_exhaustive()
    }
}
impl AccountFeed {
    pub fn new(home: &Path) -> Result<Self> {
        let (changes, _) = watch::channel(0u64);
        let sender = changes.clone();
        let watcher = repo::device_watch::watch_filtered(
            &home.join("state/device-rpc"),
            |path| {
                path.file_name().is_some_and(|name| {
                    matches!(
                        name.to_str(),
                        Some(
                            "authority.bin"
                                | "catalog.sqlite3.changed"
                                | "thread-replication.sqlite3.changed"
                        )
                    )
                })
            },
            move |result| {
                sender.send_modify(|generation| {
                    *generation = if result.is_err() {
                        u64::MAX
                    } else {
                        generation.saturating_add(1)
                    };
                });
            },
        )?;
        let result = Self {
            changes,
            watcher: Mutex::new(watcher),
            watched: Mutex::new(BTreeSet::new()),
            catalog: Mutex::new(Catalog::read(home)?),
            home: home.to_path_buf(),
        };
        result.refresh()?;
        Ok(result)
    }
    pub fn refresh(&self) -> Result<()> {
        let mut catalog = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog guard poisoned"))?;
        if catalog.is_none() {
            *catalog = Catalog::read(&self.home)?;
        }
        let Some(catalog) = catalog.as_ref() else {
            return Ok(());
        };
        let mut watched = self
            .watched
            .lock()
            .map_err(|_| anyhow::anyhow!("watch inventory poisoned"))?;
        let mut watcher = self
            .watcher
            .lock()
            .map_err(|_| anyhow::anyhow!("watch guard poisoned"))?;
        for spool in catalog.registrations()? {
            if watched.contains(&spool.heddle_dir) {
                continue;
            }
            if watched.len() >= repo::device_catalog::store::MAX_SPOOLS {
                bail!("device account watch inventory exceeds bound");
            }
            if spool.heddle_dir.try_exists()? {
                watcher.add_directory(&spool.heddle_dir)?;
                watched.insert(spool.heddle_dir);
            }
        }
        Ok(())
    }
    pub fn version(&self) -> Result<Vec<u8>> {
        let catalog = self
            .catalog
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog guard poisoned"))?;
        let generation = catalog
            .as_ref()
            .map(Catalog::generation)
            .transpose()?
            .unwrap_or(0);
        let owner = repo::device_authority::load(&self.home, chrono::Utc::now().timestamp())?;
        let event = *self.changes.borrow();
        if event == u64::MAX {
            bail!("device account watcher lost continuity");
        }
        // Projection consumers also bind exact replica generations in their own
        // snapshot, so filesystem delivery delay cannot commit a mixed view.
        use prost::Message;
        let mut hash = blake3::Hasher::new_derive_key("heddle-device-account-view-v2");
        hash.update(&generation.to_be_bytes());
        hash.update(&owner.owner.encode_to_vec());
        hash.update(&event.to_be_bytes());
        Ok(hash.finalize().as_bytes().to_vec())
    }
}
