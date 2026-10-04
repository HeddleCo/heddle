//! Bounded, repository-owned installation undo. The SQL marker decides recovery;
//! the durable intent survives process exits and failed rollback I/O.
use std::{
    cell::RefCell,
    collections::BTreeSet,
    ffi::OsStr,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use objects::{
    directory::Directory,
    lock::{RepoLock, WriteLockGuard},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{Error, Result};

const INTENT: &str = "hosted-install.intent";
const NEXT: &str = "hosted-install.next";
const RETIRED: &str = "hosted-install.retired";
const MAX_ENTRIES: usize = 1024;
const MAX_INTENT_BYTES: u64 = 1024 * 1024;
thread_local! { static ACTIVE: RefCell<BTreeSet<PathBuf>> = const { RefCell::new(BTreeSet::new()) }; }

/// A borrowed writer. Core retains the journal even if the callback moves or
/// forgets this facade. Destinations are relative to the selected heddle_dir;
/// parents must exist. This does not sandbox trusted callback code.
///
/// ```compile_fail
/// fn move_undo(writer: &mut repo::thread_replication::install_artifacts::InstallArtifacts<'_>) {
///     let _ = std::mem::take(writer); // the facade has no Default constructor
/// }
/// ```
pub struct InstallArtifacts<'a> {
    journal: &'a mut Installation,
}
impl InstallArtifacts<'_> {
    pub fn install_file(&mut self, staged: &Path, destination: &Path) -> Result<()> {
        self.journal.replace(destination, |file| {
            std::io::copy(&mut std::fs::File::open(staged)?, file)?;
            Ok(())
        })
    }
    pub fn write_file(&mut self, destination: &Path, bytes: &[u8]) -> Result<()> {
        self.journal
            .replace(destination, |file| Ok(file.write_all(bytes)?))
    }
}

/// Exclusive across authorities, processes and repository readers. Acquire
/// before native-identity.lock and the clock anchor/SQL transaction. Retain
/// guards in that order through the operation; recovery failure refuses use
/// even on an already-open repository handle.
pub(crate) struct InstallationLock {
    _guard: WriteLockGuard,
    root: Directory,
    key: PathBuf,
}
impl InstallationLock {
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        let key = directory.canonicalize()?;
        if ACTIVE.with(|active| active.borrow().contains(&key)) {
            return Err(Error::Invalid(
                "repository installation is still in progress".into(),
            ));
        }
        #[cfg(test)]
        tests::before_lock();
        let guard = RepoLock::at(key.join("locks/repo.lock"))
            .write()
            .map_err(|error| Error::Invalid(error.to_string()))?;
        let root = Directory::open(&key)?;
        recover(&root, &key)?;
        Ok(Self {
            _guard: guard,
            root,
            key,
        })
    }
    pub(super) fn recover(&self) -> Result<()> {
        if Directory::open(&self.key)?.identity()? != self.root.identity()? {
            return Err(Error::Invalid(
                "installation repository was replaced; repository unavailable".into(),
            ));
        }
        recover(&self.root, &self.key)
    }
}

/// A short-lived native reader/writer cannot outlive installation serialization.
pub(crate) struct InstallationConnection {
    connection: Connection,
    _serialization: InstallationLock,
}
impl InstallationConnection {
    pub(crate) fn open(directory: &Path, flags: rusqlite::OpenFlags) -> Result<Self> {
        let serialization = InstallationLock::acquire(directory)?;
        let connection = Connection::open_with_flags(
            directory.join(crate::local_metadata::DATABASE_NAME),
            flags,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            connection,
            _serialization: serialization,
        })
    }
}
impl std::ops::Deref for InstallationConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}
impl std::ops::DerefMut for InstallationConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    id: uuid::Uuid,
    done: bool,
    entries: Vec<Entry>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    destination: PathBuf,
    parent_identity: Vec<u8>,
    previous: bool,
    // Publication is forbidden until the backup and this bit are durable.
    prepared: bool,
}
struct HeldEntry {
    directory: Directory,
}
pub(super) struct Installation {
    root: Directory,
    key: PathBuf,
    intent: Intent,
    held: Vec<HeldEntry>,
}
impl Installation {
    pub(super) fn begin(serialization: &InstallationLock) -> Result<Self> {
        let key = serialization.key.clone();
        let root = serialization.root.try_clone()?;
        let intent = Intent {
            id: uuid::Uuid::new_v4(),
            done: false,
            entries: Vec::new(),
        };
        ACTIVE.with(|active| {
            active.borrow_mut().insert(key.clone());
        });
        Ok(Self {
            root,
            key,
            intent,
            held: Vec::new(),
        })
    }
    pub(super) fn writer(&mut self) -> InstallArtifacts<'_> {
        InstallArtifacts { journal: self }
    }
    pub(super) fn mark(&self, tx: &rusqlite::Transaction<'_>) -> Result<()> {
        if self.intent.entries.is_empty() {
            return Ok(());
        }
        if Directory::open(&self.key)?.identity()? != self.root.identity()? {
            return Err(Error::Invalid(
                "installation repository was replaced".into(),
            ));
        }
        // Recheck the parent bindings before committing. Held handles ensure
        // a parent replacement cannot redirect publication or restoration.
        for (entry, held) in self.intent.entries.iter().zip(&self.held) {
            let current = parent(&self.root, &entry.destination)?;
            if current.identity()? != held.directory.identity()? {
                return Err(Error::Invalid("installation parent was replaced".into()));
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO hosted_installation(singleton,id) VALUES(1,?1)",
            [self.intent.id.to_string()],
        )?;
        checkpoint("marker");
        Ok(())
    }
    fn replace(
        &mut self,
        destination: &Path,
        write: impl FnOnce(&mut std::fs::File) -> Result<()>,
    ) -> Result<()> {
        validate_destination(destination)?;
        let file_name = destination
            .file_name()
            .ok_or_else(|| Error::Invalid("artifact needs a file name".into()))?;
        let index = if let Some(index) = self
            .intent
            .entries
            .iter()
            .position(|e| e.destination == destination)
        {
            index
        } else {
            if self.intent.entries.len() >= MAX_ENTRIES {
                return Err(Error::Invalid("installation entry budget exceeded".into()));
            }
            let directory = parent(&self.root, destination)?;
            let previous = optional_file(&directory, file_name)?;
            let index = self.intent.entries.len();
            self.intent.entries.push(Entry {
                destination: destination.into(),
                parent_identity: directory.identity()?,
                previous: previous.is_some(),
                prepared: false,
            });
            self.held.push(HeldEntry { directory });
            save(&self.root, &self.intent)?;
            checkpoint("intent");
            if let Some(mut previous) = previous {
                let dir = &self.held[index].directory;
                let backup = artifact_name(self.intent.id, index, "backup");
                let temp = artifact_name(self.intent.id, index, "new");
                let mut file = dir.create_file(OsStr::new(&temp))?;
                // Stream the backup from the already opened no-follow file.
                // This also avoids depending on source-name stability at linkat.
                std::io::copy(&mut previous, &mut file)?;
                file.set_permissions(previous.metadata()?.permissions())?;
                file.sync_all()?;
                drop(file);
                dir.durable_rename(OsStr::new(&temp), OsStr::new(&backup))?;
                checkpoint("backup");
            }
            self.intent.entries[index].prepared = true;
            save(&self.root, &self.intent)?;
            checkpoint("prepared");
            index
        };
        let entry = &self.intent.entries[index];
        if !entry.prepared {
            return Err(Error::Invalid("artifact backup preparation failed".into()));
        }
        let directory = &self.held[index].directory;
        let temp = artifact_name(self.intent.id, index, "new");
        remove_if_present(directory, &temp)?;
        let mut file = directory.create_file(OsStr::new(&temp))?;
        write(&mut file)?;
        if let Some(previous) = optional_file(directory, file_name)? {
            file.set_permissions(previous.metadata()?.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        checkpoint("file-flush");
        directory.durable_rename(OsStr::new(&temp), file_name)?;
        checkpoint("publish");
        Ok(())
    }
    /// SQL must be rolled back/destroyed before this is called, with exclusive
    /// serialization still held. Never discard independent undo on I/O failure.
    pub(super) fn rollback(&mut self) -> Result<()> {
        if self.intent.entries.is_empty() {
            return Ok(());
        }
        reconcile(&self.root, &mut self.intent, &self.held, false)
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if self.intent.entries.is_empty() {
            return Ok(());
        }
        reconcile(&self.root, &mut self.intent, &self.held, true)
    }
}
impl Drop for Installation {
    fn drop(&mut self) {
        // No file/backup destructors: all names remain recoverable on disk.
        ACTIVE.with(|active| {
            active.borrow_mut().remove(&self.key);
        });
    }
}
fn parent(root: &Directory, destination: &Path) -> Result<Directory> {
    Ok(root.descend(
        destination
            .parent()
            .ok_or_else(|| Error::Invalid("artifact needs a parent".into()))?,
    )?)
}
fn validate_destination(destination: &Path) -> Result<()> {
    objects::directory::relative(destination)?;
    if destination.as_os_str().len() > 4096 || destination.to_str().is_none() {
        return Err(Error::Invalid(
            "installation path exceeds encoding budget".into(),
        ));
    }
    if destination.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        name.starts_with("hosted-install.")
            || name.starts_with(".hosted-install-")
            || name.starts_with(".hosted-dir-pin.")
            || name == "locks"
            || name == crate::local_metadata::DATABASE_NAME
            || name.starts_with("metadata.sqlite3-")
    }) {
        return Err(Error::Invalid(
            "artifact overlaps repository transaction state".into(),
        ));
    }
    Ok(())
}
fn artifact_name(id: uuid::Uuid, index: usize, kind: &str) -> String {
    format!(".hosted-install-{id}-{index}.{kind}")
}
fn optional_file(dir: &Directory, name: &OsStr) -> Result<Option<std::fs::File>> {
    match dir.open_file(name) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn remove_if_present(dir: &Directory, name: &str) -> Result<()> {
    match dir.remove_file(OsStr::new(name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
fn save(root: &Directory, intent: &Intent) -> Result<()> {
    let bytes = serde_json::to_vec(intent).map_err(|error| Error::Invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_INTENT_BYTES {
        return Err(Error::Invalid("installation intent budget exceeded".into()));
    }
    remove_if_present(root, NEXT)?;
    let mut file = root.create_file(OsStr::new(NEXT))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    root.durable_rename(OsStr::new(NEXT), OsStr::new(INTENT))?;
    Ok(())
}
fn read(root: &Directory) -> Result<Option<Intent>> {
    let Some(file) = optional_file(root, OsStr::new(INTENT))? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.take(MAX_INTENT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INTENT_BYTES {
        return Err(Error::Invalid("installation intent budget exceeded".into()));
    }
    let intent: Intent =
        serde_json::from_slice(&bytes).map_err(|e| Error::Invalid(e.to_string()))?;
    if intent.entries.len() > MAX_ENTRIES {
        return Err(Error::Invalid("installation entry budget exceeded".into()));
    }
    let mut seen = BTreeSet::new();
    for entry in &intent.entries {
        validate_destination(&entry.destination)?;
        if !seen.insert(&entry.destination) {
            return Err(Error::Invalid("duplicate installation destination".into()));
        }
    }
    Ok(Some(intent))
}
fn recover(root: &Directory, directory: &Path) -> Result<()> {
    let Some(mut intent) = read(root)? else {
        return Ok(());
    };
    let connection = Connection::open_with_flags(
        directory.join(crate::local_metadata::DATABASE_NAME),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let marker_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='hosted_installation')",
        [],
        |r| r.get(0),
    )?;
    let committed = marker_table
        && connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM hosted_installation WHERE id=?1)",
            [intent.id.to_string()],
            |r| r.get::<_, bool>(0),
        )?;
    let mut held = Vec::new();
    for entry in &intent.entries {
        let dir = parent(root, &entry.destination)?;
        if dir.identity()? != entry.parent_identity {
            return Err(Error::Invalid(
                "installation recovery parent was replaced; repository unavailable".into(),
            ));
        }
        held.push(HeldEntry { directory: dir });
    }
    reconcile(root, &mut intent, &held, committed)
}
fn reconcile(
    root: &Directory,
    intent: &mut Intent,
    held: &[HeldEntry],
    committed: bool,
) -> Result<()> {
    if !intent.done {
        let mut failure = None;
        if !committed {
            for (index, (entry, held)) in intent.entries.iter().zip(held).enumerate().rev() {
                if !entry.prepared {
                    continue;
                }
                let result = (|| {
                    let dir = &held.directory;
                    let destination = entry
                        .destination
                        .file_name()
                        .ok_or_else(|| Error::Invalid("artifact needs a file name".into()))?;
                    if entry.previous {
                        rollback_fault(index, "rename")?;
                        let backup = artifact_name(intent.id, index, "backup");
                        let restore = artifact_name(intent.id, index, "restore");
                        remove_if_present(dir, &restore)?;
                        // Never consume the durable backup. A crash after restore
                        // but before saving done must be safe to replay.
                        dir.hard_link(OsStr::new(&backup), OsStr::new(&restore))?;
                        dir.durable_rename(OsStr::new(&restore), destination)?;
                    } else {
                        rollback_fault(index, "unlink")?;
                        retire(
                            dir,
                            destination,
                            &artifact_name(intent.id, index, "deleted"),
                        )?;
                    }
                    checkpoint("rollback");
                    Ok(())
                })();
                if let Err(error) = result
                    && failure.is_none()
                {
                    failure = Some(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        intent.done = true;
        if let Err(error) = save(root, intent) {
            intent.done = false;
            return Err(error);
        }
        checkpoint("done");
    }
    let mut failure = None;
    for (index, held) in held.iter().enumerate() {
        for kind in ["backup", "new", "restore", "deleted"] {
            let name = artifact_name(intent.id, index, kind);
            if let Err(error) = retire(
                &held.directory,
                OsStr::new(&name),
                &format!("{name}.retired"),
            ) && failure.is_none()
            {
                failure = Some(error);
            }
            checkpoint("cleanup");
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    // A lost tombstone unlink is harmless: it cannot republish an artifact or
    // undo an installation. Publication of the tombstone is the durable removal
    // of the active intent, and happens only after durable backup retirement.
    root.durable_rename(OsStr::new(INTENT), OsStr::new(RETIRED))?;
    checkpoint("retired");
    remove_if_present(root, RETIRED)?;
    remove_if_present(root, NEXT)?;
    Ok(())
}
fn retire(dir: &Directory, source: &OsStr, tombstone: &str) -> Result<()> {
    match dir.durable_rename(source, OsStr::new(tombstone)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    remove_if_present(dir, tombstone)
}

#[cfg(not(test))]
pub(super) fn checkpoint(_: &str) {}
#[cfg(test)]
pub(super) fn checkpoint(step: &str) {
    tests::checkpoint(step);
}
#[cfg(not(test))]
fn rollback_fault(_: usize, _: &'static str) -> Result<()> {
    Ok(())
}
#[cfg(test)]
fn rollback_fault(index: usize, operation: &'static str) -> Result<()> {
    tests::rollback_fault(index, operation)
}
#[cfg(test)]
pub(super) mod tests;
