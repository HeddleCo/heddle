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
