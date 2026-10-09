// SPDX-License-Identifier: Apache-2.0
//! Shared worktree walking infrastructure.

use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use objects::{
    error::{HeddleError, Result},
    object::{ContentHash, Tree, TreeEntry},
    worktree::{is_reserved_directory_child, reserved_worktree_write},
};

use crate::{
    repository::Repository,
    reserved_worktree_paths::note_skipped_reserved_path,
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_index::{IndexEntry as CachedWorktreeEntry, IndexEntryKind as CachedEntryKind},
};

/// Per-blob capture cap shared with the mount's write-extent cap
/// (`mount::core`), so a file servable over FUSE is always capturable.
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct WalkEntry<'a> {
    pub(crate) path: &'a Path,
    pub(crate) name: &'a str,
    pub(crate) metadata: fs::Metadata,
    pub(crate) executable: bool,
}

#[derive(Debug)]
pub(crate) struct WalkDirectory<'a> {
    pub(crate) rel_path: &'a Path,
}

#[derive(Debug)]
pub(crate) struct ListedDirEntry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) kind: ListedDirEntryKind,
    metadata: Option<fs::Metadata>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ListedDirEntryKind {
    File { executable: Option<bool> },
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy)]
struct WalkLocation<'a> {
    dir: &'a Path,
    key: &'a str,
}

pub(crate) trait WorktreeWalkPolicy {
    type DirectoryState;
    type Output;

    fn prefetch_entry_metadata(&self, _tree: Option<&Tree>) -> bool {
        false
    }

    fn skip_directory_before_enumeration(
        &mut self,
        rel_path: &Path,
        metadata: &fs::Metadata,
        tree: Option<&Tree>,
    ) -> Result<Option<Self::Output>> {
        let _ = (rel_path, metadata, tree);
        Ok(None)
    }

    fn enter_directory(
        &mut self,
        directory: &WalkDirectory<'_>,
        tree: Option<&Tree>,
    ) -> Result<Self::DirectoryState>;

    fn reuse_tree_entry_before_metadata(
        &mut self,
        _rel_path: &Path,
        _tree_entry: &TreeEntry,
        _state: &mut Self::DirectoryState,
    ) -> Result<bool> {
        Ok(false)
    }

    fn cached_tree_for_entry(&self, _rel_path: &Path, _tree_hash: &ContentHash) -> Option<Tree> {
        None
    }

    fn visit_file(
        &mut self,
        entry: WalkEntry<'_>,
        tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()>;

    fn visit_symlink(
        &mut self,
        entry: WalkEntry<'_>,
        tree_entry: Option<&TreeEntry>,
        state: &mut Self::DirectoryState,
    ) -> Result<()>;

    fn visit_directory_output(
        &mut self,
        entry: WalkEntry<'_>,
        tree_entry: Option<&TreeEntry>,
        output: Self::Output,
        state: &mut Self::DirectoryState,
    ) -> Result<()>;

    fn visit_missing(
        &mut self,
        rel_path: &Path,
        tree_entry: &TreeEntry,
        state: &mut Self::DirectoryState,
    ) -> Result<()>;

    fn leave_directory(
        &mut self,
        directory: &WalkDirectory<'_>,
        tree: Option<&Tree>,
        state: Self::DirectoryState,
    ) -> Result<Self::Output>;

    fn should_check_missing(&self, _tree: Option<&Tree>, _state: &Self::DirectoryState) -> bool {
        true
    }

    fn should_walk_entries(&self, _tree: Option<&Tree>, _state: &Self::DirectoryState) -> bool {
        true
    }
}

pub(crate) fn walk_worktree<P: WorktreeWalkPolicy>(
    repo: &Repository,
    dir: &Path,
    ignore_matcher: &WorktreeIgnoreMatcher,
    tree: Option<&Tree>,
    policy: &mut P,
) -> Result<P::Output> {
    let root_key = String::new();
    // `dir` (the walk root), not `repo.root()`, is the base that relative
    // paths are keyed against, so a dedicated worktree such as
    // `capture_thread_from_disk`'s thread checkout walks like a root.
    walk_directory(
        repo,
        dir,
        WalkLocation {
            dir,
            key: &root_key,
        },
        None,
        ignore_matcher,
        tree,
        policy,
    )
}

fn walk_directory<P: WorktreeWalkPolicy>(
    repo: &Repository,
    base: &Path,
    location: WalkLocation<'_>,
    known_metadata: Option<fs::Metadata>,
    ignore_matcher: &WorktreeIgnoreMatcher,
    tree: Option<&Tree>,
    policy: &mut P,
) -> Result<P::Output> {
    let dir = location.dir;
    let metadata = match known_metadata {
        Some(metadata) => metadata,
        None => dir.symlink_metadata()?,
    };
    let rel_path = relative_path(base, dir);
    if let Some(output) = policy.skip_directory_before_enumeration(&rel_path, &metadata, tree)? {
        return Ok(output);
    }
    let dir_entries = list_directory(dir, policy.prefetch_entry_metadata(tree), |_| true)?;
    let directory = WalkDirectory {
        rel_path: &rel_path,
    };
    let tree_entries = tree.map(Tree::entries).unwrap_or(&[]);
    let mut next_tree_entry = 0usize;

    let mut state = policy.enter_directory(&directory, tree)?;

    let should_walk_entries = policy.should_walk_entries(tree, &state);
    let check_missing = policy.should_check_missing(tree, &state) && should_walk_entries;
    if should_walk_entries {
        let mut entry_key = location.key.to_string();
        for entry in &dir_entries {
            let name = entry.name.as_str();
            // Reserved-path hard-deny (heddle#1413): root `.heddle/` is
            // identity/engine state. User ignore rules cannot un-ignore it.
            if is_reserved_directory_child(directory.rel_path, name) {
                note_skipped_reserved_path(&directory.rel_path.join(name));
                continue;
            }
            // A `.gitmodules` symlink is never recorded either (heddle#2028).
            if matches!(entry.kind, ListedDirEntryKind::Symlink) {
                let path = directory.rel_path.join(name);
                if reserved_worktree_write(&path, true).is_some() {
                    note_skipped_reserved_path(&path);
                    continue;
                }
            }
            if ignore_matcher.should_prune_directory_child(directory.rel_path, name) {
                continue;
            }
            // Nested-thread-worktree exclusion: skip directories that
            // are recorded as another thread's execution path. The
            // matcher only walks its precomputed list when populated,
            // so the cost is zero on flat single-thread layouts and
            // O(N_other_threads) per descended directory in the
            // demo-style nested case.
            if ignore_matcher.should_prune_absolute_path(&entry.path) {
                continue;
            }
            push_key_component(&mut entry_key, name);
            while check_missing
                && next_tree_entry < tree_entries.len()
                && tree_entries[next_tree_entry].name() < name
            {
                let missing_entry = &tree_entries[next_tree_entry];
                if !is_reserved_tree_entry(directory.rel_path, missing_entry) {
                    policy.visit_missing(
                        &directory.rel_path.join(missing_entry.name()),
                        missing_entry,
                        &mut state,
                    )?;
                }
                next_tree_entry += 1;
            }
            let tree_entry = tree_entries
                .get(next_tree_entry)
                .filter(|entry| entry.name() == name);
            if tree_entry.is_some() {
                next_tree_entry += 1;
            }

            if let Some(tree_entry) = tree_entry
                && policy.reuse_tree_entry_before_metadata(
                    &directory.rel_path.join(name),
                    tree_entry,
                    &mut state,
                )?
            {
                pop_key_component(&mut entry_key, location.key);
                continue;
            }

            let metadata = match &entry.metadata {
                Some(metadata) => metadata.clone(),
                None => entry.path.symlink_metadata()?,
            };

            let result = match entry.kind {
                ListedDirEntryKind::Symlink => policy.visit_symlink(
                    WalkEntry {
                        path: &entry.path,
                        name,
                        metadata,
                        executable: false,
                    },
                    tree_entry,
                    &mut state,
                ),
                ListedDirEntryKind::File { executable } => policy.visit_file(
                    WalkEntry {
                        path: &entry.path,
                        name,
                        executable: executable.unwrap_or_else(|| is_executable(&metadata)),
                        metadata,
                    },
                    tree_entry,
                    &mut state,
                ),
                ListedDirEntryKind::Directory => {
                    let child_rel_path = directory.rel_path.join(name);
                    let subtree = tree_entry
                        .filter(|entry| entry.is_tree())
                        .and_then(TreeEntry::tree_hash)
                        .map(
                            |hash| match policy.cached_tree_for_entry(&child_rel_path, &hash) {
                                Some(tree) => Ok(Some(tree)),
                                None => repo.require_tree(&hash).map(Some),
                            },
                        )
                        .transpose()?
                        .flatten();
                    let output = walk_directory(
                        repo,
                        base,
                        WalkLocation {
                            dir: &entry.path,
                            key: &entry_key,
                        },
                        Some(metadata.clone()),
                        ignore_matcher,
                        subtree.as_ref(),
                        policy,
                    )?;
                    policy.visit_directory_output(
                        WalkEntry {
                            path: &entry.path,
                            name,
                            metadata,
                            executable: false,
                        },
                        tree_entry,
                        output,
                        &mut state,
                    )
                }
                ListedDirEntryKind::Other => Ok(()),
            };
            pop_key_component(&mut entry_key, location.key);
            result?;
        }
    }

    if check_missing {
        for entry in &tree_entries[next_tree_entry..] {
            if !is_reserved_tree_entry(directory.rel_path, entry) {
                policy.visit_missing(&directory.rel_path.join(entry.name()), entry, &mut state)?;
            }
        }
    }

    policy.leave_directory(&directory, tree, state)
}

/// Whether a recorded tree entry is one checkout never writes
/// (heddle#2028). Its absence from the worktree is not a deletion: the walk
/// skips it on both sides, so status stays clean after such a checkout.
fn is_reserved_tree_entry(parent: &Path, entry: &TreeEntry) -> bool {
    reserved_worktree_write(&parent.join(entry.name()), entry.is_symlink()).is_some()
}

pub(crate) fn list_directory(
    dir: &Path,
    prefetch_metadata: bool,
    keep: impl Fn(&str) -> bool,
) -> Result<Vec<ListedDirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if !keep(name) {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type()?;
            let (kind, metadata) = if file_type.is_symlink() {
                (ListedDirEntryKind::Symlink, None)
            } else {
                let kind = if file_type.is_file() {
                    ListedDirEntryKind::File { executable: None }
                } else if file_type.is_dir() {
                    ListedDirEntryKind::Directory
                } else {
                    ListedDirEntryKind::Other
                };
                let metadata = if prefetch_metadata {
                    Some(entry.metadata()?)
                } else {
                    None
                };
                let kind = if let Some(metadata) = metadata.as_ref() {
                    if metadata.is_file() {
                        ListedDirEntryKind::File {
                            executable: Some(is_executable(metadata)),
                        }
                    } else if metadata.is_dir() {
                        ListedDirEntryKind::Directory
                    } else {
                        ListedDirEntryKind::Other
                    }
                } else {
                    kind
                };
                (kind, metadata)
            };
            entries.push(ListedDirEntry {
                name: name.to_string(),
                path,
                kind,
                metadata,
            });
        }
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn push_key_component(key: &mut String, name: &str) {
    if !key.is_empty() {
        key.push('/');
    }
    key.push_str(name);
}

fn pop_key_component(key: &mut String, parent_key: &str) {
    key.truncate(parent_key.len());
}

fn relative_path(base: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

pub(crate) fn cache_key(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    if lossy.contains('\\') {
        lossy.replace('\\', "/")
    } else {
        lossy.into_owned()
    }
}

pub(crate) fn modified_parts(metadata: &fs::Metadata) -> Option<(i64, u32)> {
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((
        i64::try_from(duration.as_secs()).ok()?,
        duration.subsec_nanos(),
    ))
}

pub(crate) fn build_cached_entry(
    hash: ContentHash,
    metadata: &fs::Metadata,
    executable: bool,
    kind: CachedEntryKind,
) -> Option<CachedWorktreeEntry> {
    let (modified_sec, modified_nsec) = modified_parts(metadata)?;
    Some(CachedWorktreeEntry {
        hash,
        size: metadata.len(),
        modified_sec,
        modified_nsec,
        executable,
        kind,
    })
}

fn read_file_content(path: &Path, size: u64) -> Result<Vec<u8>> {
    if size > MAX_FILE_SIZE {
        return Err(HeddleError::InvalidFileSize(size));
    }
    let mut file = File::open(path)?;
    let mut content = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        content.extend_from_slice(&buffer[..read]);
        if content.len() as u64 > MAX_FILE_SIZE {
            return Err(HeddleError::InvalidFileSize(content.len() as u64));
        }
    }
    Ok(content)
}

pub(crate) fn read_file_hash(path: &Path, size: u64) -> Result<ContentHash> {
    if size > MAX_FILE_SIZE {
        return Err(HeddleError::InvalidFileSize(size));
    }

    let mut file = File::open(path)?;
    let mut hasher = ContentHash::typed_hasher("blob", size);
    let mut buffer = [0_u8; 8192];
    let mut bytes_read = 0_u64;

    while bytes_read < size {
        let remaining = usize::try_from((size - bytes_read).min(buffer.len() as u64)).unwrap_or(0);
        let read = file.read(&mut buffer[..remaining])?;
        if read == 0 {
            break;
        }
        bytes_read += read as u64;
        hasher.update(&buffer[..read]);
    }

    let extra_read = file.read(&mut buffer[..1])?;
    if bytes_read == size && extra_read == 0 {
        return Ok(ContentHash::from_bytes(hasher.finalize().into()));
    }

    let content = read_file_content(path, size)?;
    Ok(ContentHash::compute_typed("blob", &content))
}

pub(crate) fn read_blob_with_hash(
    path: &Path,
    size: u64,
) -> Result<(objects::object::Blob, ContentHash)> {
    let content = read_file_content(path, size)?;
    let hash = ContentHash::compute_typed("blob", &content);
    Ok((objects::object::Blob::new(content), hash))
}
