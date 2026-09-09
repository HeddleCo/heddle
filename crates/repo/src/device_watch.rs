//! OS change notifications for local device observations. A shared watcher feeds
//! every subscription for a Spool. Errors require a reset, never silent staleness.
use std::path::Path;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

pub struct DeviceWatch {
    _watcher: RecommendedWatcher,
}
impl DeviceWatch {
    pub fn add_directory(&mut self, path: &Path) -> Result<()> {
        self._watcher.watch(path, RecursiveMode::NonRecursive)?;
        Ok(())
    }
    pub fn add(&mut self, path: &Path) -> Result<()> {
        self._watcher.watch(path, RecursiveMode::Recursive)?;
        Ok(())
    }
}
impl std::fmt::Debug for DeviceWatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DeviceWatch")
    }
}
pub fn watch(
    path: &Path,
    notify: impl Fn(Result<(), String>) + Send + 'static,
) -> Result<DeviceWatch> {
    watch_filtered(path, |_| true, notify)
}
pub fn watch_filtered(
    path: &Path,
    accept: impl Fn(&Path) -> bool + Send + 'static,
    notify: impl Fn(Result<(), String>) + Send + 'static,
) -> Result<DeviceWatch> {
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event)
                if !matches!(event.kind, notify::EventKind::Access(_))
                    && event.paths.iter().any(|path| accept(path)) =>
            {
                notify(Ok(()))
            }
            Ok(_) => {}
            Err(error) => notify(Err(error.to_string())),
        })?;
    watcher.watch(path, RecursiveMode::Recursive)?;
    Ok(DeviceWatch { _watcher: watcher })
}

/// Hold the existing replica WAL open while its OS feed is observed. Otherwise
/// opening and closing a read can create/remove WAL files and wake itself.
#[derive(Debug)]
pub struct DeviceDatabaseWatch {
    _connection: std::sync::Mutex<rusqlite::Connection>,
}
pub fn hold_replica_database(heddle_dir: &std::path::Path) -> anyhow::Result<DeviceDatabaseWatch> {
    let connection = rusqlite::Connection::open_with_flags(
        heddle_dir.join(crate::local_metadata::DATABASE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let _: i64 = connection.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    Ok(DeviceDatabaseWatch {
        _connection: std::sync::Mutex::new(connection),
    })
}

/// Shared invalidation cursor, separate from RPC section/replay cursors. Reading
/// only the committed high watermark is constant work even after a large burst.
/// A gap invalidates the whole projection; consumers rebuild their snapshots.
#[derive(Debug)]
pub struct MetadataInvalidation {
    connection: rusqlite::Connection,
    cursor: i64,
}
impl MetadataInvalidation {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = crate::local_metadata::open_existing(path)?
            .ok_or_else(|| anyhow::anyhow!("metadata database is absent"))?;
        let cursor = Self::position(&connection)?.1;
        Ok(Self { connection, cursor })
    }
    fn position(connection: &rusqlite::Connection) -> Result<(i64, i64)> {
        Ok(connection.query_row(
            "SELECT floor,COALESCE((SELECT MAX(cursor) FROM metadata_changes),floor) FROM metadata_change_state WHERE singleton=1",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }
    pub fn changed(&mut self) -> Result<bool> {
        let (floor, high) = Self::position(&self.connection)?;
        // A replaced/regressed store also requires a fresh authoritative view.
        let changed = high != self.cursor || self.cursor < floor;
        self.cursor = high;
        Ok(changed)
    }
}

/// One committed-change gate for all subscribers to a Spool. Filesystem and
/// authority changes remain independent wake sources. No observer owns a SQL
/// polling loop or a transaction; the notify worker performs this bounded read.
pub fn watch_with_metadata(
    path: &Path,
    metadata: &Path,
    accept: impl Fn(&Path) -> bool + Send + 'static,
    notify: impl Fn(Result<(), String>) + Send + 'static,
) -> Result<DeviceWatch> {
    let mut gate = MetadataInvalidation::open(metadata)?;
    let metadata = metadata.to_path_buf();
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) if !matches!(event.kind, notify::EventKind::Access(_)) => {
                let mut durable = false;
                let mut filesystem = false;
                for path in event.paths.iter().filter(|path| accept(path)) {
                    if path.parent() == Some(metadata.as_path())
                        && path.file_name().is_some_and(|name| {
                            name == crate::local_metadata::DATABASE_NAME
                                || name == crate::local_metadata::CHANGE_MARKER_NAME
                        })
                    {
                        durable = true;
                    } else {
                        filesystem = true;
                    }
                }
                if durable {
                    match gate.changed() {
                        Ok(changed) => filesystem |= changed,
                        Err(error) => {
                            notify(Err(error.to_string()));
                            return;
                        }
                    }
                }
                if filesystem {
                    notify(Ok(()));
                }
            }
            Ok(_) => {}
            Err(error) => notify(Err(error.to_string())),
        })?;
    watcher.watch(path, RecursiveMode::Recursive)?;
    Ok(DeviceWatch { _watcher: watcher })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_invalidation_observes_commit_not_rollback_or_duplicate_wakes() {
        let directory = tempfile::tempdir().expect("store");
        let mut writer = crate::local_metadata::open(directory.path()).expect("schema");
        let mut gate = MetadataInvalidation::open(directory.path()).expect("gate");
        assert!(!gate.changed().expect("unchanged"));
        for commit in [false, true] {
            let tx = writer.transaction().expect("transaction");
            tx.execute(
                "INSERT INTO runs(id,thread,record) VALUES('run','thread',x'01')",
                [],
            )
            .expect("run");
            assert!(!gate.changed().expect("uncommitted write invisible"));
            if commit {
                tx.commit().expect("commit");
            } else {
                tx.rollback().expect("rollback");
            }
            assert_eq!(gate.changed().expect("commit wake"), commit);
            assert!(!gate.changed().expect("duplicate wake"));
        }
        let tx = writer.transaction().expect("burst");
        for id in 0..crate::local_metadata::CHANGE_WINDOW + 2 {
            tx.execute(
                "INSERT INTO runs(id,thread,record) VALUES(?1,'thread',x'01')",
                [id.to_string()],
            )
            .expect("burst run");
        }
        tx.commit().expect("burst commit");
        assert!(gate.changed().expect("expired window rebuilds snapshot"));
        assert!(!gate.changed().expect("gap was consumed"));
        writer
            .execute("DELETE FROM runs WHERE id='run'", [])
            .expect("delete");
        assert!(gate.changed().expect("deletion invalidates"));
    }
}
