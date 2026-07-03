// SPDX-License-Identifier: Apache-2.0
//! Typed tree path resolution with per-caller leaf policies.

use std::path::{Component, Path};

use super::{Blob, ContentHash, Tree, TreeEntry};
use crate::error::HeddleError;
use crate::store::ObjectSource;

/// How a tree-path walk classifies and materializes the terminal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafPolicy {
    /// Return the terminal [`TreeEntry`] regardless of entry type (provenance).
    Entry,
    /// Return the blob content hash at the terminal path; symlinks are excluded (redact).
    BlobOnly,
    /// Load the terminal blob via [`TreeEntry::leaf_content_hash`], including symlinks
    /// (staleness).
    LeafContentBlob,
}

/// Successful resolution of a path within a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTreeTarget {
    pub entry: TreeEntry,
    pub content_hash: Option<ContentHash>,
    pub blob: Option<Blob>,
}

/// Errors surfaced by [`resolve_tree_path`] that callers map to their own messages.
#[derive(Debug)]
pub enum TreePathResolveError {
    Store {
        hash: ContentHash,
        source: Box<HeddleError>,
    },
    SubtreeMissing(ContentHash),
}

impl std::error::Error for TreePathResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TreePathResolveError::Store { source, .. } => Some(source.as_ref()),
            TreePathResolveError::SubtreeMissing(_) => None,
        }
    }
}

impl From<TreePathResolveError> for HeddleError {
    fn from(value: TreePathResolveError) -> Self {
        match value {
            TreePathResolveError::Store { source, .. } => *source,
            TreePathResolveError::SubtreeMissing(hash) => {
                HeddleError::InvalidObject(format!("subtree {} missing from store", hash.short()))
            }
        }
    }
}

impl std::fmt::Display for TreePathResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreePathResolveError::Store { hash, .. } => {
                write!(f, "failed to load tree {}", hash.short())
            }
            TreePathResolveError::SubtreeMissing(hash) => {
                write!(f, "subtree {} missing from store", hash.short())
            }
        }
    }
}

/// Split a repository-relative path into its first component and the remainder.
pub fn split_path(path: &Path) -> Option<(&str, &Path)> {
    let mut components = path.components();
    let first = components.next()?;
    let Component::Normal(name) = first else {
        return None;
    };
    Some((name.to_str()?, components.as_path()))
}

/// Walk `path` from `root` through nested subtrees and resolve the terminal entry
/// according to `policy`.
///
/// `Ok(None)` means the path is absent or terminates at the wrong entry type for the
/// policy. Store failures and missing subtrees are policy-dependent; see
/// [`TreePathResolveError`].
pub fn resolve_tree_path<S: ObjectSource>(
    store: &S,
    root: &ContentHash,
    path: &Path,
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    let Some(segments) = segments_for_policy(path, policy) else {
        return Ok(None);
    };
    if segments.is_empty() {
        return Ok(None);
    }

    let Some(tree) = load_subtree(store, root, policy)? else {
        return Ok(None);
    };
    resolve_from_tree(store, &tree, &segments, policy)
}

#[cfg(feature = "async-source")]
pub async fn resolve_tree_path_async<S: crate::store::AsyncObjectSource + ?Sized>(
    store: &S,
    root: &ContentHash,
    path: &Path,
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    let Some(segments) = segments_for_policy(path, policy) else {
        return Ok(None);
    };
    if segments.is_empty() {
        return Ok(None);
    }

    let Some(tree) = load_subtree_async(store, root, policy).await? else {
        return Ok(None);
    };
    resolve_from_tree_async(store, &tree, &segments, policy).await
}

fn segments_for_policy(path: &Path, policy: LeafPolicy) -> Option<Vec<String>> {
    match policy {
        LeafPolicy::Entry => path_segments(path),
        LeafPolicy::BlobOnly => {
            let path_str = path.to_str()?;
            Some(
                path_str
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect(),
            )
        }
        LeafPolicy::LeafContentBlob => Some(
            path.to_string_lossy()
                .split('/')
                .map(str::to_string)
                .collect(),
        ),
    }
}

fn path_segments(path: &Path) -> Option<Vec<String>> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => segments.push(name.to_str()?.to_string()),
            _ => return None,
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments)
}

fn resolve_from_tree<S: ObjectSource>(
    store: &S,
    tree: &Tree,
    segments: &[String],
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    let name = segments[0].as_str();
    let Some(entry) = tree.get(name) else {
        return Ok(None);
    };

    if segments.len() == 1 {
        return resolve_leaf(store, entry.clone(), policy);
    }

    if !entry.is_tree() {
        return Ok(None);
    }
    let Some(tree_hash) = entry.tree_hash() else {
        return Ok(None);
    };
    let Some(subtree) = load_subtree(store, &tree_hash, policy)? else {
        return Ok(None);
    };
    resolve_from_tree(store, &subtree, &segments[1..], policy)
}

#[cfg(feature = "async-source")]
async fn resolve_from_tree_async<S: crate::store::AsyncObjectSource + ?Sized>(
    store: &S,
    tree: &Tree,
    segments: &[String],
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    let name = segments[0].as_str();
    let Some(entry) = tree.get(name) else {
        return Ok(None);
    };

    if segments.len() == 1 {
        return resolve_leaf_async(store, entry.clone(), policy).await;
    }

    if !entry.is_tree() {
        return Ok(None);
    }
    let Some(tree_hash) = entry.tree_hash() else {
        return Ok(None);
    };
    let Some(subtree) = load_subtree_async(store, &tree_hash, policy).await? else {
        return Ok(None);
    };
    Box::pin(resolve_from_tree_async(store, &subtree, &segments[1..], policy)).await
}

fn resolve_leaf<S: ObjectSource>(
    store: &S,
    entry: TreeEntry,
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    match policy {
        LeafPolicy::Entry => {
            let content_hash = entry_content_hash(&entry);
            Ok(Some(ResolvedTreeTarget {
                entry,
                content_hash,
                blob: None,
            }))
        }
        LeafPolicy::BlobOnly => {
            let Some(content_hash) = entry.blob_hash() else {
                return Ok(None);
            };
            Ok(Some(ResolvedTreeTarget {
                entry,
                content_hash: Some(content_hash),
                blob: None,
            }))
        }
        LeafPolicy::LeafContentBlob => {
            let Some(content_hash) = entry.leaf_content_hash() else {
                return Ok(None);
            };
            let blob = match store.get_blob(&content_hash) {
                Ok(Some(blob)) => Some(blob),
                Ok(None) => None,
                Err(source) => {
                    return Err(TreePathResolveError::Store {
                        hash: content_hash,
                        source: Box::new(source),
                    });
                }
            };
            Ok(blob.map(|blob| ResolvedTreeTarget {
                entry,
                content_hash: Some(content_hash),
                blob: Some(blob),
            }))
        }
    }
}

#[cfg(feature = "async-source")]
async fn resolve_leaf_async<S: crate::store::AsyncObjectSource + ?Sized>(
    store: &S,
    entry: TreeEntry,
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    match policy {
        LeafPolicy::Entry => {
            let content_hash = entry_content_hash(&entry);
            Ok(Some(ResolvedTreeTarget {
                entry,
                content_hash,
                blob: None,
            }))
        }
        LeafPolicy::BlobOnly => {
            let Some(content_hash) = entry.blob_hash() else {
                return Ok(None);
            };
            Ok(Some(ResolvedTreeTarget {
                entry,
                content_hash: Some(content_hash),
                blob: None,
            }))
        }
        LeafPolicy::LeafContentBlob => {
            let Some(content_hash) = entry.leaf_content_hash() else {
                return Ok(None);
            };
            let blob = match store.get_blob(&content_hash).await {
                Ok(Some(blob)) => Some(blob),
                Ok(None) => None,
                Err(source) => {
                    return Err(TreePathResolveError::Store {
                        hash: content_hash,
                        source: Box::new(source),
                    });
                }
            };
            Ok(blob.map(|blob| ResolvedTreeTarget {
                entry,
                content_hash: Some(content_hash),
                blob: Some(blob),
            }))
        }
    }
}

fn entry_content_hash(entry: &TreeEntry) -> Option<ContentHash> {
    entry
        .content_hash()
        .or_else(|| entry.tree_hash())
        .or_else(|| entry.leaf_content_hash())
}

fn load_subtree<S: ObjectSource>(
    store: &S,
    hash: &ContentHash,
    policy: LeafPolicy,
) -> std::result::Result<Option<Tree>, TreePathResolveError> {
    match policy {
        LeafPolicy::Entry => Ok(store.get_tree(hash).ok().flatten()),
        LeafPolicy::LeafContentBlob => match store.get_tree(hash) {
            Ok(tree) => Ok(tree),
            Err(source) => Err(TreePathResolveError::Store {
                hash: *hash,
                source: Box::new(source),
            }),
        },
        LeafPolicy::BlobOnly => match store.get_tree(hash) {
            Ok(Some(tree)) => Ok(Some(tree)),
            Ok(None) => Err(TreePathResolveError::SubtreeMissing(*hash)),
            Err(source) => Err(TreePathResolveError::Store {
                hash: *hash,
                source: Box::new(source),
            }),
        },
    }
}

#[cfg(feature = "async-source")]
async fn load_subtree_async<S: crate::store::AsyncObjectSource + ?Sized>(
    store: &S,
    hash: &ContentHash,
    policy: LeafPolicy,
) -> std::result::Result<Option<Tree>, TreePathResolveError> {
    match policy {
        LeafPolicy::Entry => Ok(store.get_tree(hash).await.ok().flatten()),
        LeafPolicy::LeafContentBlob => match store.get_tree(hash).await {
            Ok(tree) => Ok(tree),
            Err(source) => Err(TreePathResolveError::Store {
                hash: *hash,
                source: Box::new(source),
            }),
        },
        LeafPolicy::BlobOnly => match store.get_tree(hash).await {
            Ok(Some(tree)) => Ok(Some(tree)),
            Ok(None) => Err(TreePathResolveError::SubtreeMissing(*hash)),
            Err(source) => Err(TreePathResolveError::Store {
                hash: *hash,
                source: Box::new(source),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{EntryType, TreeEntry};
    use crate::store::{InMemoryStore, ObjectStore};

    fn create_blob(store: &InMemoryStore, content: &[u8]) -> ContentHash {
        ObjectStore::put_blob(store, &Blob::from_slice(content)).unwrap()
    }

    fn create_tree(
        store: &InMemoryStore,
        entries: Vec<(&str, ContentHash, EntryType)>,
    ) -> ContentHash {
        let entries = entries
            .into_iter()
            .map(|(name, hash, entry_type)| match entry_type {
                EntryType::Blob => TreeEntry::file(name.to_string(), hash, false),
                EntryType::Tree => TreeEntry::directory(name.to_string(), hash),
                EntryType::Symlink => TreeEntry::symlink(name.to_string(), hash),
                EntryType::Gitlink => unreachable!("tree path tests do not build gitlinks"),
                EntryType::Spoollink => {
                    unreachable!("tree path tests do not build spoollinks")
                }
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        ObjectStore::put_tree(store, &Tree::from_entries(entries)).unwrap()
    }

    struct Fixture {
        store: InMemoryStore,
        root: ContentHash,
        blob_hash: ContentHash,
        symlink_hash: ContentHash,
        nested_blob_hash: ContentHash,
        missing_subtree_hash: ContentHash,
    }

    fn fixture() -> Fixture {
        let store = InMemoryStore::new();
        let blob_hash = create_blob(&store, b"blob content");
        let symlink_hash = create_blob(&store, b"target.txt");
        let nested_blob_hash = create_blob(&store, b"nested content");

        let nested_tree = create_tree(
            &store,
            vec![("inner.txt", nested_blob_hash, EntryType::Blob)],
        );
        let missing_subtree_hash = ContentHash::compute(b"not-in-store");
        let missing_subtree_parent = create_tree(
            &store,
            vec![("ghost", missing_subtree_hash, EntryType::Tree)],
        );
        let root = create_tree(
            &store,
            vec![
                ("file.txt", blob_hash, EntryType::Blob),
                ("link", symlink_hash, EntryType::Symlink),
                ("dir", nested_tree, EntryType::Tree),
                ("missing", missing_subtree_parent, EntryType::Tree),
            ],
        );

        Fixture {
            store,
            root,
            blob_hash,
            symlink_hash,
            nested_blob_hash,
            missing_subtree_hash,
        }
    }

    #[test]
    fn leaf_content_blob_resolves_symlinks_and_nested_paths() {
        let fx = fixture();

        let file = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("file.txt"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .unwrap();
        assert_eq!(file.content_hash, Some(fx.blob_hash));
        assert_eq!(file.blob.as_ref().unwrap().content(), b"blob content");

        let link = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("link"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .unwrap();
        assert_eq!(link.content_hash, Some(fx.symlink_hash));
        assert_eq!(link.blob.as_ref().unwrap().content(), b"target.txt");

        let nested = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("dir/inner.txt"),
            LeafPolicy::LeafContentBlob,
        )
        .unwrap()
        .unwrap();
        assert_eq!(nested.content_hash, Some(fx.nested_blob_hash));

        assert!(
            resolve_tree_path(
                &fx.store,
                &fx.root,
                Path::new("dir"),
                LeafPolicy::LeafContentBlob,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            resolve_tree_path(
                &fx.store,
                &fx.root,
                Path::new("nope.txt"),
                LeafPolicy::LeafContentBlob,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            resolve_tree_path(
                &fx.store,
                &fx.root,
                Path::new("missing/ghost/inner.txt"),
                LeafPolicy::LeafContentBlob,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn entry_policy_returns_terminal_entry_for_any_leaf_type() {
        let fx = fixture();

        let file = resolve_tree_path(&fx.store, &fx.root, Path::new("file.txt"), LeafPolicy::Entry)
            .unwrap()
            .unwrap();
        assert_eq!(file.entry.blob_hash(), Some(fx.blob_hash));

        let link = resolve_tree_path(&fx.store, &fx.root, Path::new("link"), LeafPolicy::Entry)
            .unwrap()
            .unwrap();
        assert!(link.entry.is_symlink());
        assert_eq!(link.entry.leaf_content_hash(), Some(fx.symlink_hash));

        let dir = resolve_tree_path(&fx.store, &fx.root, Path::new("dir"), LeafPolicy::Entry)
            .unwrap()
            .unwrap();
        assert!(dir.entry.is_tree());

        assert!(
            resolve_tree_path(&fx.store, &fx.root, Path::new("dir/missing"), LeafPolicy::Entry)
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_tree_path(
                &fx.store,
                &fx.root,
                Path::new("missing/ghost/inner.txt"),
                LeafPolicy::Entry,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn blob_only_excludes_symlinks_and_errors_on_missing_subtree() {
        let fx = fixture();

        let file = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("file.txt"),
            LeafPolicy::BlobOnly,
        )
        .unwrap()
        .unwrap();
        assert_eq!(file.content_hash, Some(fx.blob_hash));

        assert!(
            resolve_tree_path(&fx.store, &fx.root, Path::new("link"), LeafPolicy::BlobOnly)
                .unwrap()
                .is_none()
        );

        let nested = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("dir/inner.txt"),
            LeafPolicy::BlobOnly,
        )
        .unwrap()
        .unwrap();
        assert_eq!(nested.content_hash, Some(fx.nested_blob_hash));

        assert!(
            resolve_tree_path(&fx.store, &fx.root, Path::new("dir"), LeafPolicy::BlobOnly)
                .unwrap()
                .is_none()
        );

        let err = resolve_tree_path(
            &fx.store,
            &fx.root,
            Path::new("missing/ghost/inner.txt"),
            LeafPolicy::BlobOnly,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TreePathResolveError::SubtreeMissing(hash) if hash == fx.missing_subtree_hash
        ));
    }
}