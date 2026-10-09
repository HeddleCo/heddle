// SPDX-License-Identifier: Apache-2.0
//! Typed tree path resolution with per-caller leaf policies.

use std::{
    borrow::Cow,
    path::{Component, Path},
};

#[cfg(feature = "async-source")]
use super::AsyncObjectSource;
use super::{Blob, ContentHash, ObjectSource, Tree, TreeEntry};
use crate::error::HeddleError;

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
            TreePathResolveError::SubtreeMissing(hash) => HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_hex(),
            },
        }
    }
}

impl std::fmt::Display for TreePathResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreePathResolveError::Store { hash, source } => {
                write!(f, "failed to load object {}: {source}", hash.short())
            }
            TreePathResolveError::SubtreeMissing(hash) => {
                write!(f, "missing tree object: {hash} is not available locally")
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
/// policy. Referenced objects must exist for every policy; missing trees and
/// blobs return errors instead of masquerading as absent paths.
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

    let tree = load_subtree(store, root)?;
    resolve_from_tree(store, &tree, &segments, policy)
}

#[cfg(feature = "async-source")]
pub async fn resolve_tree_path_async<S: AsyncObjectSource + ?Sized>(
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

    let tree = load_subtree_async(store, root).await?;
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
    let mut current_tree = Cow::Borrowed(tree);

    for (index, segment) in segments.iter().enumerate() {
        let Some(entry) = current_tree.get(segment) else {
            return Ok(None);
        };

        if index + 1 == segments.len() {
            return resolve_leaf(store, entry.clone(), policy);
        }

        let Some(tree_hash) = entry.tree_hash() else {
            return Ok(None);
        };
        let subtree = load_subtree(store, &tree_hash)?;
        current_tree = Cow::Owned(subtree);
    }

    Ok(None)
}

#[cfg(feature = "async-source")]
async fn resolve_from_tree_async<S: AsyncObjectSource + ?Sized>(
    store: &S,
    tree: &Tree,
    segments: &[String],
    policy: LeafPolicy,
) -> std::result::Result<Option<ResolvedTreeTarget>, TreePathResolveError> {
    let mut current_tree = Cow::Borrowed(tree);

    for (index, segment) in segments.iter().enumerate() {
        let Some(entry) = current_tree.get(segment) else {
            return Ok(None);
        };

        if index + 1 == segments.len() {
            return resolve_leaf_async(store, entry.clone(), policy).await;
        }

        let Some(tree_hash) = entry.tree_hash() else {
            return Ok(None);
        };
        let subtree = load_subtree_async(store, &tree_hash).await?;
        current_tree = Cow::Owned(subtree);
    }

    Ok(None)
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
            let blob = store.require_blob(&content_hash).map_err(|source| {
                TreePathResolveError::Store {
                    hash: content_hash,
                    source: Box::new(source),
                }
            })?;
            Ok(Some(ResolvedTreeTarget {
                entry,
                content_hash: Some(content_hash),
                blob: Some(blob),
            }))
        }
    }
}

#[cfg(feature = "async-source")]
async fn resolve_leaf_async<S: AsyncObjectSource + ?Sized>(
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
            let blob = store.require_blob(&content_hash).await.map_err(|source| {
                TreePathResolveError::Store {
                    hash: content_hash,
                    source: Box::new(source),
                }
            })?;
            Ok(Some(ResolvedTreeTarget {
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
) -> std::result::Result<Tree, TreePathResolveError> {
    match store.get_tree(hash) {
        Ok(Some(tree)) => Ok(tree),
        Ok(None) => Err(TreePathResolveError::SubtreeMissing(*hash)),
        Err(source) => Err(TreePathResolveError::Store {
            hash: *hash,
            source: Box::new(source),
        }),
    }
}

#[cfg(feature = "async-source")]
async fn load_subtree_async<S: AsyncObjectSource + ?Sized>(
    store: &S,
    hash: &ContentHash,
) -> std::result::Result<Tree, TreePathResolveError> {
    match store.get_tree(hash).await {
        Ok(Some(tree)) => Ok(tree),
        Ok(None) => Err(TreePathResolveError::SubtreeMissing(*hash)),
        Err(source) => Err(TreePathResolveError::Store {
            hash: *hash,
            source: Box::new(source),
        }),
    }
}
