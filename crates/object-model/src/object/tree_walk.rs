// SPDX-License-Identifier: Apache-2.0
//! Tree integrity walking — single traversal for reference and content checks.

use std::collections::HashSet;

use crate::error::Result;

use super::{ContentHash, ObjectSource, Tree, TreeEntry};

/// Events emitted while walking reachable trees for integrity checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeIntegrityEvent<'a> {
    /// A tree was entered for the first time during this walk.
    EnterTree { hash: ContentHash, tree: &'a Tree },
    /// A blob file entry at `path` (symlinks and gitlinks are excluded).
    BlobLeaf { entry: &'a TreeEntry, path: String },
    /// A child tree entry from `parent_hash`.
    TreeRef {
        parent_hash: ContentHash,
        entry: &'a TreeEntry,
    },
}

/// Walk all trees reachable from `roots`, deduplicating visited trees.
///
/// Missing root or subtree trees are skipped silently. Gitlink entries are not
/// descended into. Visitation order is depth-first, sorted tree entry order.
pub fn walk_tree_integrity<S, V>(
    source: &S,
    roots: impl IntoIterator<Item = ContentHash>,
    visitor: &mut V,
) -> Result<()>
where
    S: ObjectSource + ?Sized,
    V: FnMut(TreeIntegrityEvent<'_>) -> Result<()>,
{
    let mut visited = HashSet::new();
    for root in roots {
        walk_tree_recursive(source, &root, "", &mut visited, visitor)?;
    }
    Ok(())
}

fn walk_tree_recursive<S, V>(
    source: &S,
    tree_hash: &ContentHash,
    path_prefix: &str,
    visited: &mut HashSet<ContentHash>,
    visitor: &mut V,
) -> Result<()>
where
    S: ObjectSource + ?Sized,
    V: FnMut(TreeIntegrityEvent<'_>) -> Result<()>,
{
    if visited.contains(tree_hash) {
        return Ok(());
    }
    visited.insert(*tree_hash);

    let Some(tree) = source.get_tree(tree_hash)? else {
        return Ok(());
    };

    visitor(TreeIntegrityEvent::EnterTree {
        hash: *tree_hash,
        tree: &tree,
    })?;

    for entry in tree.entries() {
        let path = if path_prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{path_prefix}/{}", entry.name())
        };

        if entry.blob_hash().is_some() {
            visitor(TreeIntegrityEvent::BlobLeaf {
                entry,
                path: path.clone(),
            })?;
        } else if let Some(child_hash) = entry.tree_hash() {
            visitor(TreeIntegrityEvent::TreeRef {
                parent_hash: *tree_hash,
                entry,
            })?;
            walk_tree_recursive(source, &child_hash, &path, visited, visitor)?;
        }
    }

    Ok(())
}
