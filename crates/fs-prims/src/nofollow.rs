// SPDX-License-Identifier: Apache-2.0
//! No-follow worktree writes (heddle#2017).
//!
//! Tracked symlinks are checked out exactly as Git checks them out, whatever
//! their target: absolute, `../`-escaping, or dangling. Creating a symlink
//! writes nothing outside the checkout.
//!
//! The hazard is *other* writes that traverse one. A malicious repository can
//! track `link -> /home/user/.ssh` in one state and `link/authorized_keys` in a
//! later one, or a user can leave an untracked or ignored symlink where a
//! later state tracks a directory. A writer that opens `root/link/file` with
//! ordinary path syscalls follows `link` and writes outside the checkout.
//!
//! The rule these helpers enforce, matching Git's checkout: **no worktree
//! write or removal traverses a symlink below the checkout root.**
//!
//! * Directories are created one component at a time with `lstat` + `mkdir`.
//!   A symlink where a directory belongs is unlinked (which never touches its
//!   target) and replaced by a real directory, as `git checkout` does for
//!   tracked and ignored paths.
//! * Leaf files are created with `O_CREAT | O_EXCL | O_NOFOLLOW` after the old
//!   leaf is unlinked, so a symlink at the leaf is replaced, not written
//!   through.
//! * Removals skip a path whose parent is a symlink or missing, as Git's
//!   `has_symlink_or_noent_leading_path` does. The tracked path is not in the
//!   worktree, and whatever the symlink points at is not ours to delete.
//!
//! The checkout root itself is trusted. It may legitimately sit beneath
//! symlinked ancestors (`/var -> /private/var` on macOS, a symlinked home).
//!
//! These checks defend against repository *content*. They do not defend
//! against a concurrent local process that swaps directories for symlinks
//! between the check and the write; such a process can already write the
//! worktree directly.

use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use crate::fs_atomic::enrich_fs_error;

/// The plain-name components of `path` beneath `root`. Tree entry names never
/// contain `..` or a root, and a path built from one that did must not be
/// written.
fn components_beneath<'a>(root: &Path, path: &'a Path) -> io::Result<Vec<&'a OsStr>> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "worktree path {} is outside checkout {}",
                path.display(),
                root.display()
            ),
        )
    })?;
    let mut names = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "worktree path {} escapes checkout {}",
                        path.display(),
                        root.display()
                    ),
                ));
            }
        }
    }
    Ok(names)
}

/// Creates and verifies directories beneath a checkout root without ever
/// traversing a symlink.
///
/// Verified directories are cached, so one instance should cover one batch of
/// writes in which nothing turns a verified directory back into a symlink. A
/// batch that also writes symlinks must write them after its other leaves and
/// recheck with [`refuse_symlinked_parent`].
pub struct NoFollowDirectories<'a> {
    root: &'a Path,
    verified: HashSet<PathBuf>,
}

impl<'a> NoFollowDirectories<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            verified: HashSet::new(),
        }
    }

    /// Make `dir` and every directory between it and the root real
    /// directories. Missing ones are created, a symlink in the way is replaced
    /// by a directory, and any other non-directory is an error.
    pub fn ensure(&mut self, dir: &Path) -> io::Result<()> {
        let mut current = self.root.to_path_buf();
        for name in components_beneath(self.root, dir)? {
            current.push(name);
            if self.verified.contains(&current) {
                continue;
            }
            ensure_real_directory(&current)?;
            self.verified.insert(current.clone());
        }
        Ok(())
    }

    /// [`Self::ensure`] the directory that will hold the leaf `path`.
    pub fn ensure_parent(&mut self, path: &Path) -> io::Result<()> {
        let names = components_beneath(self.root, path)?;
        if names.len() <= 1 {
            return Ok(());
        }
        match path.parent() {
            Some(parent) => self.ensure(parent),
            None => Ok(()),
        }
    }

    /// Whether every directory between the root and the leaf `path` is a real
    /// directory. Creates nothing. `false` when one is a symlink, some other
    /// non-directory, or missing.
    pub fn parent_is_real_directory(&mut self, path: &Path) -> io::Result<bool> {
        let names = components_beneath(self.root, path)?;
        let Some((_, parents)) = names.split_last() else {
            return Ok(true);
        };
        let mut current = self.root.to_path_buf();
        for name in parents {
            current.push(name);
            if self.verified.contains(&current) {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() => {
                    self.verified.insert(current.clone());
                }
                Ok(_) => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(enrich_fs_error(&current, "inspecting", error)),
            }
        }
        Ok(true)
    }
}

/// Fail if any directory between `root` and the leaf `path` is a symlink.
/// Uncached: run immediately before the write it guards.
pub fn refuse_symlinked_parent(root: &Path, path: &Path) -> io::Result<()> {
    let names = components_beneath(root, path)?;
    let Some((_, parents)) = names.split_last() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for name in parents {
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| enrich_fs_error(&current, "inspecting", error))?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "refusing to write {} through symlink {}: worktree writes never follow symlinks",
                path.strip_prefix(root).unwrap_or(path).display(),
                current.strip_prefix(root).unwrap_or(&current).display(),
            )));
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            // Unlinking a symlink removes the link, never its target. Windows
            // directory symlinks need `remove_dir`.
            fs::remove_file(path)
                .or_else(|_| fs::remove_dir(path))
                .map_err(|error| enrich_fs_error(path, "removing", error))?;
            create_directory(path)
        }
        Ok(_) => Err(enrich_fs_error(
            path,
            "creating",
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a file occupies a path that must be a directory",
            ),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_directory(path),
        Err(error) => Err(enrich_fs_error(path, "inspecting", error)),
    }
}

fn create_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        // A concurrent creator won; accept it only if it made a real directory.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => Ok(()),
                _ => Err(enrich_fs_error(path, "creating", error)),
            }
        }
        Err(error) => Err(enrich_fs_error(path, "creating", error)),
    }
}

/// Unlink a file or symlink at `path`. Leaves a directory in place, so the
/// create that follows fails rather than writing into it.
fn unlink_leaf(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() => fs::remove_file(path)
            .or_else(|error| {
                // A Windows directory symlink.
                if metadata.file_type().is_symlink() {
                    fs::remove_dir(path)
                } else {
                    Err(error)
                }
            })
            .map_err(|error| enrich_fs_error(path, "removing", error)),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(enrich_fs_error(path, "inspecting", error)),
    }
}

/// Create a new regular file at `path`, refusing to follow a symlink there.
/// Fails if anything already occupies the name.
pub fn create_new_nofollow(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Open an existing file for metadata changes without following a symlink at
/// `path`.
pub fn open_existing_nofollow(path: &Path) -> io::Result<File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Unlink any file or symlink at `path`, then create a fresh regular file
/// there without following symlinks. The parent must already be verified.
pub fn create_replacing_leaf(path: &Path) -> io::Result<File> {
    unlink_leaf(path)?;
    create_new_nofollow(path).map_err(|error| enrich_fs_error(path, "writing", error))
}

/// Write `bytes` to the tracked file `path` beneath `root`: parents become
/// real directories, a leaf symlink is replaced, nothing is followed.
pub fn write_file_beneath(root: &Path, path: &Path, bytes: &[u8]) -> io::Result<File> {
    NoFollowDirectories::new(root).ensure_parent(path)?;
    let mut file = create_replacing_leaf(path)?;
    file.write_all(bytes)
        .map_err(|error| enrich_fs_error(path, "writing", error))?;
    Ok(file)
}

/// Create the symlink `path -> target` beneath `root`, replacing any file or
/// symlink already there. Parents become real directories; nothing is
/// followed.
#[cfg(unix)]
pub fn write_symlink_beneath(root: &Path, path: &Path, target: &OsStr) -> io::Result<()> {
    NoFollowDirectories::new(root).ensure_parent(path)?;
    unlink_leaf(path)?;
    std::os::unix::fs::symlink(target, path)
        .map_err(|error| enrich_fs_error(path, "creating symlink", error))
}

/// Remove the tracked leaf `path` beneath `root` (a file or a symlink, never
/// its target). Returns `false` without touching anything when a parent is a
/// symlink or missing, matching Git: the path is not in the worktree.
pub fn remove_leaf_beneath(root: &Path, path: &Path) -> io::Result<bool> {
    if !NoFollowDirectories::new(root).parent_is_real_directory(path)? {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(false),
        Ok(_) => {
            unlink_leaf(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(enrich_fs_error(path, "inspecting", error)),
    }
}

/// Recursively remove `path` beneath `root` without following symlinks, in
/// the path itself or in any parent. Skips (returns `false`) when a parent is
/// a symlink or missing.
pub fn remove_path_beneath(root: &Path, path: &Path) -> io::Result<bool> {
    if !NoFollowDirectories::new(root).parent_is_real_directory(path)? {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(_) => {
            crate::fs_ops::remove_path_recursively(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(enrich_fs_error(path, "inspecting", error)),
    }
}

/// Upper bound on an in-tree control file such as `.heddleignore` or
/// `.gitignore`. Real ignore files are a few KiB; anything larger is refused
/// rather than read into memory.
pub const MAX_IN_TREE_CONTROL_FILE_BYTES: u64 = 1 << 20;

/// What reading an in-tree control file found.
#[derive(Debug, PartialEq, Eq)]
pub enum InTreeControlFile {
    Absent,
    Contents(Vec<u8>),
    /// A symlink, which is never followed. Git ≥ 2.32 refuses to follow
    /// in-tree `.gitignore` symlinks for the same reason: the target may be
    /// any file on the machine, or a device such as `/dev/zero`.
    Symlink,
    /// A FIFO, device, socket or directory. Reading one could block forever
    /// or never end.
    NotRegular,
    TooLarge(u64),
}

/// Read a control file the repository itself supplies (`.heddleignore`,
/// `.gitignore`): only a regular file, opened without following a symlink,
/// and only up to `max_bytes`.
pub fn read_in_tree_control_file(path: &Path, max_bytes: u64) -> io::Result<InTreeControlFile> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InTreeControlFile::Absent);
        }
        Err(error) => return Err(enrich_fs_error(path, "inspecting", error)),
    };
    if metadata.file_type().is_symlink() {
        return Ok(InTreeControlFile::Symlink);
    }
    if !metadata.is_file() {
        return Ok(InTreeControlFile::NotRegular);
    }
    if metadata.len() > max_bytes {
        return Ok(InTreeControlFile::TooLarge(metadata.len()));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK: if the name was swapped for a FIFO since the lstat, the
        // open must not block waiting for a writer.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InTreeControlFile::Absent);
        }
        Err(error) => return Err(enrich_fs_error(path, "reading", error)),
    };
    if !file.metadata()?.is_file() {
        return Ok(InTreeControlFile::NotRegular);
    }
    let mut contents = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(file, max_bytes + 1), &mut contents)
        .map_err(|error| enrich_fs_error(path, "reading", error))?;
    if contents.len() as u64 > max_bytes {
        return Ok(InTreeControlFile::TooLarge(contents.len() as u64));
    }
    Ok(InTreeControlFile::Contents(contents))
}

/// Prepare `path` beneath `root` for an ordinary read-modify-write that must
/// not touch a user's symlinks: missing parent directories are created, but an
/// existing symlinked parent, or a symlink or non-regular file at the leaf, is
/// refused rather than followed or replaced. Used where the path is a
/// convention inside the repository (`.claude/settings.json`) that a tracked
/// symlink may legitimately redirect elsewhere.
pub fn prepare_regular_file_beneath(root: &Path, path: &Path) -> io::Result<()> {
    let names = components_beneath(root, path)?;
    let Some((_, parents)) = names.split_last() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for name in parents {
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::other(format!(
                    "refusing to write {} through symlink {}",
                    path.strip_prefix(root).unwrap_or(path).display(),
                    current.strip_prefix(root).unwrap_or(&current).display(),
                )));
            }
            Ok(_) => {
                return Err(enrich_fs_error(
                    &current,
                    "creating",
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "a file occupies a path that must be a directory",
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => create_directory(&current)?,
            Err(error) => return Err(enrich_fs_error(&current, "inspecting", error)),
        }
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::other(format!(
            "refusing to write {}: it is a symlink",
            path.strip_prefix(root).unwrap_or(path).display(),
        ))),
        Ok(_) => Err(io::Error::other(format!(
            "refusing to write {}: it is not a regular file",
            path.strip_prefix(root).unwrap_or(path).display(),
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(enrich_fs_error(path, "inspecting", error)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use super::*;

    #[test]
    fn ensure_replaces_symlinked_parent_with_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();

        write_file_beneath(root.path(), &root.path().join("link/sub/file"), b"x").unwrap();

        assert!(
            fs::symlink_metadata(root.path().join("link"))
                .unwrap()
                .is_dir()
        );
        assert_eq!(fs::read(root.path().join("link/sub/file")).unwrap(), b"x");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn write_replaces_leaf_symlink_instead_of_following_it() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        symlink(&victim, root.path().join("leaf")).unwrap();

        write_file_beneath(root.path(), &root.path().join("leaf"), b"new").unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(
            fs::symlink_metadata(root.path().join("leaf"))
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn removals_never_reach_through_a_symlinked_parent() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("file"), b"keep").unwrap();
        fs::create_dir(outside.path().join("dir")).unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();

        assert!(!remove_leaf_beneath(root.path(), &root.path().join("link/file")).unwrap());
        assert!(!remove_path_beneath(root.path(), &root.path().join("link/dir")).unwrap());

        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"keep");
        assert!(outside.path().join("dir").is_dir());
    }

    #[test]
    fn refuse_symlinked_parent_names_the_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();

        let error = refuse_symlinked_parent(root.path(), &root.path().join("link/leaf"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("through symlink link"), "{error}");
    }

    #[test]
    fn paths_outside_or_escaping_the_root_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        assert!(write_file_beneath(root.path(), Path::new("/elsewhere/file"), b"x").is_err());
        assert!(write_file_beneath(root.path(), &root.path().join("../escape"), b"x").is_err());
    }

    #[test]
    fn symlink_beneath_writes_arbitrary_targets_exactly() {
        let root = tempfile::tempdir().unwrap();
        for (name, target) in [("abs", "/usr/share/dict/words"), ("rel", "../../outside")] {
            write_symlink_beneath(
                root.path(),
                &root.path().join("d").join(name),
                target.as_ref(),
            )
            .unwrap();
            assert_eq!(
                fs::read_link(root.path().join("d").join(name)).unwrap(),
                Path::new(target)
            );
        }
    }
}
