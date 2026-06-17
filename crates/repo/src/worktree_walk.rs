// SPDX-License-Identifier: Apache-2.0
//! Shared worktree walking infrastructure.

use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    time::UNIX_EPOCH,
};

use objects::{
    error::{HeddleError, Result},
    object::{ContentHash, Tree, TreeEntry},
    store::ObjectStore,
};

use crate::{
    repository::Repository,
    worktree_ignore::WorktreeIgnoreMatcher,
    worktree_index::{IndexEntry as CachedWorktreeEntry, IndexEntryKind as CachedEntryKind},
};

const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

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
    // The `base` argument is what `validate_symlink_target` uses
    // as the allowed root for symlink-escape checks. Use `dir`
    // (the walk root), not `repo.root()`, so symlinks inside a
    // dedicated worktree like `capture_thread_from_disk`'s
    // materialised thread path validate against that path.
    //
    // Pre-fix, base was always `repo.root()`. For the common
    // `build_tree(self.root)` case the two are identical so
    // behaviour is unchanged. For `build_tree(thread_path)` —
    // used by `capture_thread_from_disk` on a dedicated thread
    // worktree — `dir` and `repo.root()` diverge and the old
    // wiring rejected *every* symlink inside the thread tree as
    // "outside the repo", breaking `thread switch` auto-capture
    // for any thread that contained a symlink.
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
    let dir_entries = list_directory(dir, policy.prefetch_entry_metadata(tree))?;
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
                && tree_entries[next_tree_entry].name.as_str() < name
            {
                let missing_entry = &tree_entries[next_tree_entry];
                policy.visit_missing(
                    &directory.rel_path.join(&missing_entry.name),
                    missing_entry,
                    &mut state,
                )?;
                next_tree_entry += 1;
            }
            let tree_entry = tree_entries
                .get(next_tree_entry)
                .filter(|entry| entry.name == name);
            if tree_entry.is_some() {
                next_tree_entry += 1;
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
                    let output = walk_directory(
                        repo,
                        base,
                        WalkLocation {
                            dir: &entry.path,
                            key: &entry_key,
                        },
                        Some(metadata.clone()),
                        ignore_matcher,
                        tree_entry
                            .filter(|entry| entry.is_tree())
                            .map(|entry| repo.store().get_tree(&entry.hash))
                            .transpose()?
                            .flatten()
                            .as_ref(),
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
            policy.visit_missing(&directory.rel_path.join(&entry.name), entry, &mut state)?;
        }
    }

    policy.leave_directory(&directory, tree, state)
}

pub(crate) fn list_directory(dir: &Path, prefetch_metadata: bool) -> Result<Vec<ListedDirEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
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

pub(crate) fn validate_symlink_target(base: &Path, symlink_dir: &Path, target: &Path) -> bool {
    let canonical_base = match base.canonicalize() {
        Ok(base) => base,
        Err(_) => return false,
    };
    let canonical_symlink_dir = match symlink_dir.canonicalize() {
        Ok(dir) => dir,
        Err(_) => return false,
    };
    if !canonical_symlink_dir.starts_with(&canonical_base) {
        return false;
    }
    let target_path = if target.is_absolute() {
        target.to_path_buf()
    } else {
        canonical_symlink_dir.join(target)
    };
    if let Ok(resolved) = target_path.canonicalize() {
        return resolved.starts_with(&canonical_base);
    }
    dangling_path_stays_within_base(&canonical_base, &target_path)
}

fn dangling_path_stays_within_base(base: &Path, path: &Path) -> bool {
    let mut existing = path.to_path_buf();
    let mut missing_components = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return false;
        };
        missing_components.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return false;
        };
        existing = parent.to_path_buf();
    }

    let Ok(mut resolved) = existing.canonicalize() else {
        return false;
    };
    if !resolved.starts_with(base) {
        return false;
    }
    for component in missing_components.iter().rev() {
        resolved.push(component);
    }
    path_stays_within_base_lexically(base, &resolved)
}

fn path_stays_within_base_lexically(base: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(base) else {
        return false;
    };
    let mut depth = 0_usize;
    for component in relative.components() {
        match component {
            Component::ParentDir if depth == 0 => return false,
            Component::ParentDir => depth -= 1,
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::validate_symlink_target;

    #[test]
    #[cfg(unix)]
    fn validate_symlink_target_rejects_dangling_target_through_escaping_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();

        assert!(!validate_symlink_target(
            root.path(),
            root.path(),
            Path::new("escape/missing")
        ));
    }

    #[test]
    #[cfg(unix)]
    fn validate_symlink_target_allows_dangling_target_under_real_in_repo_dir() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("inside")).unwrap();

        assert!(validate_symlink_target(
            root.path(),
            root.path(),
            Path::new("inside/missing")
        ));
    }
}
