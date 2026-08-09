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
    /// A root or referenced subtree could not be loaded.
    MissingTree {
        hash: ContentHash,
        parent_hash: Option<ContentHash>,
        path: String,
    },
}

/// Walk all trees reachable from `roots`, deduplicating visited trees.
///
/// Missing root or subtree trees emit [`TreeIntegrityEvent::MissingTree`].
/// Gitlink entries are not descended into. Visitation order is depth-first,
/// sorted tree entry order. The implementation uses explicit frames so tree
/// depth cannot overflow the process stack.
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
        walk_tree_iterative(source, root, &mut visited, visitor)?;
    }
    Ok(())
}

struct WalkFrame {
    hash: ContentHash,
    tree: Tree,
    path_prefix: String,
    next_entry: usize,
}

fn walk_tree_iterative<S, V>(
    source: &S,
    root_hash: ContentHash,
    visited: &mut HashSet<ContentHash>,
    visitor: &mut V,
) -> Result<()>
where
    S: ObjectSource + ?Sized,
    V: FnMut(TreeIntegrityEvent<'_>) -> Result<()>,
{
    if !visited.insert(root_hash) {
        return Ok(());
    }

    let Some(root_tree) = source.get_tree(&root_hash)? else {
        visitor(TreeIntegrityEvent::MissingTree {
            hash: root_hash,
            parent_hash: None,
            path: String::new(),
        })?;
        return Ok(());
    };

    visitor(TreeIntegrityEvent::EnterTree {
        hash: root_hash,
        tree: &root_tree,
    })?;

    let mut stack = vec![WalkFrame {
        hash: root_hash,
        tree: root_tree,
        path_prefix: String::new(),
        next_entry: 0,
    }];

    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.tree.entries().get(frame.next_entry).cloned() else {
            stack.pop();
            continue;
        };
        frame.next_entry += 1;

        let path = if frame.path_prefix.is_empty() {
            entry.name().to_string()
        } else {
            format!("{}/{}", frame.path_prefix, entry.name())
        };

        if entry.blob_hash().is_some() {
            visitor(TreeIntegrityEvent::BlobLeaf {
                entry: &entry,
                path,
            })?;
        } else if let Some(child_hash) = entry.tree_hash() {
            visitor(TreeIntegrityEvent::TreeRef {
                parent_hash: frame.hash,
                entry: &entry,
            })?;

            if !visited.insert(child_hash) {
                continue;
            }
            let Some(child_tree) = source.get_tree(&child_hash)? else {
                visitor(TreeIntegrityEvent::MissingTree {
                    hash: child_hash,
                    parent_hash: Some(frame.hash),
                    path,
                })?;
                continue;
            };
            visitor(TreeIntegrityEvent::EnterTree {
                hash: child_hash,
                tree: &child_tree,
            })?;
            stack.push(WalkFrame {
                hash: child_hash,
                tree: child_tree,
                path_prefix: path,
                next_entry: 0,
            });
        }
    }

    Ok(())
}
