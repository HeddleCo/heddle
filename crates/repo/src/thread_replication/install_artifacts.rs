//! Filesystem writes owned by a hosted installation transaction.
use std::path::{Path, PathBuf};

use super::{Error, Result};

/// The install callback must write packs, pins and sidecar metadata through
/// this journal. Paths must have existing parents. SQLite metadata belongs in
/// the enclosing trust transaction, never in this file journal.
/// Successful writes remain visible for commit; rejection restores the exact
/// previous files before releasing the shared trust serialization.
#[derive(Default)]
pub struct InstallArtifacts {
    files: Vec<(PathBuf, Option<tempfile::NamedTempFile>)>,
}

impl InstallArtifacts {
    /// Install a staged file without reading a pack into memory.
    pub fn install_file(&mut self, staged: &Path, destination: &Path) -> Result<()> {
        self.replace(destination, |file| {
            std::io::copy(&mut std::fs::File::open(staged)?, file)?;
            Ok(())
        })
    }

    /// Atomically replace a pin or sidecar while retaining its previous bytes
    /// and permissions for rollback. Repeated writes retain the first version.
    pub fn write_file(&mut self, destination: &Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        self.replace(destination, |file| Ok(file.write_all(bytes)?))
    }

    fn replace(
        &mut self,
        destination: &Path,
        write: impl FnOnce(&mut std::fs::File) -> Result<()>,
    ) -> Result<()> {
        let parent = destination.parent().ok_or_else(|| {
            Error::Invalid("install artifact requires an existing parent directory".into())
        })?;
        let destination = parent.canonicalize()?.join(
            destination
                .file_name()
                .ok_or_else(|| Error::Invalid("install artifact requires a file name".into()))?,
        );
        // Symlinks, directories and SQLite's active files cannot be journaled.
        if destination.file_name().is_some_and(|name| {
            name == crate::local_metadata::DATABASE_NAME
                || name == "metadata.sqlite3-wal"
                || name == "metadata.sqlite3-shm"
        }) {
            return Err(Error::Invalid(
                "SQLite is owned by the trust transaction".into(),
            ));
        }
        let metadata = match std::fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_file() => Some(metadata),
            Ok(_) => {
                return Err(Error::Invalid(
                    "install artifact must be a regular file".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        write(replacement.as_file_mut())?;
        replacement.as_file().sync_all()?;
        if !self.files.iter().any(|(path, _)| path == &destination) {
            let backup = if let Some(metadata) = metadata {
                let backup = tempfile::NamedTempFile::new_in(parent)?;
                // Hard links retain large existing packs without another copy.
                std::fs::remove_file(backup.path())?;
                std::fs::hard_link(&destination, backup.path())?;
                replacement
                    .as_file()
                    .set_permissions(metadata.permissions())?;
                Some(backup)
            } else {
                None
            };
            self.files.push((destination.clone(), backup));
        }
        replacement
            .persist(&destination)
            .map_err(|error| error.error)?;
        Ok(())
    }

    pub(super) fn rollback(&mut self) -> Result<()> {
        while let Some((destination, backup)) = self.files.pop() {
            if let Some(backup) = backup {
                backup.persist(&destination).map_err(|error| error.error)?;
            } else {
                match std::fs::remove_file(destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}
