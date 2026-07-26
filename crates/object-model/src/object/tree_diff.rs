// SPDX-License-Identifier: Apache-2.0
//! Shared tree-to-tree diffing implementation.
//!
//! This module provides a generic tree diffing algorithm that can be used
//! by both `repo` and `semantic` crates.

use std::ops::ControlFlow;

#[cfg(feature = "async-source")]
use super::AsyncObjectSource;
use super::{ContentHash, DiffKind, FileChange, FileChangeSet, ObjectSource, Tree};

struct DiffFrame {
    from: Option<Tree>,
    to: Option<Tree>,
    prefix: String,
    from_index: usize,
    to_index: usize,
}

enum DiffStep {
    Emit(FileChange),
    Descend {
        from_hash: Option<ContentHash>,
        to_hash: Option<ContentHash>,
        name: String,
    },
    Done,
}

fn advance_merge(frame: &mut DiffFrame) -> DiffStep {
    let from_entries = frame.from.as_ref().map_or(&[][..], Tree::entries);
    let to_entries = frame.to.as_ref().map_or(&[][..], Tree::entries);

    loop {
        match (
            from_entries.get(frame.from_index),
            to_entries.get(frame.to_index),
        ) {
            (Some(from_entry), Some(to_entry)) => match from_entry.name().cmp(to_entry.name()) {
                std::cmp::Ordering::Less => {
                    frame.from_index += 1;
                    if let Some(from_hash) = from_entry.tree_hash() {
                        return DiffStep::Descend {
                            from_hash: Some(from_hash),
                            to_hash: None,
                            name: from_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, from_entry.name()),
                        DiffKind::Deleted,
                    ));
                }
                std::cmp::Ordering::Greater => {
                    frame.to_index += 1;
                    if let Some(to_hash) = to_entry.tree_hash() {
                        return DiffStep::Descend {
                            from_hash: None,
                            to_hash: Some(to_hash),
                            name: to_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, to_entry.name()),
                        DiffKind::Added,
                    ));
                }
                std::cmp::Ordering::Equal => {
                    frame.from_index += 1;
                    frame.to_index += 1;
                    if from_entry.target() == to_entry.target() {
                        continue;
                    }
                    if let (Some(from_hash), Some(to_hash)) =
                        (from_entry.tree_hash(), to_entry.tree_hash())
                    {
                        return DiffStep::Descend {
                            from_hash: Some(from_hash),
                            to_hash: Some(to_hash),
                            name: to_entry.name().to_owned(),
                        };
                    }
                    return DiffStep::Emit(FileChange::new(
                        child_path(&frame.prefix, to_entry.name()),
                        DiffKind::Modified,
                    ));
                }
            },
            (Some(from_entry), None) => {
                frame.from_index += 1;
                if let Some(from_hash) = from_entry.tree_hash() {
                    return DiffStep::Descend {
                        from_hash: Some(from_hash),
                        to_hash: None,
                        name: from_entry.name().to_owned(),
                    };
                }
                return DiffStep::Emit(FileChange::new(
                    child_path(&frame.prefix, from_entry.name()),
                    DiffKind::Deleted,
                ));
            }
            (None, Some(to_entry)) => {
                frame.to_index += 1;
                if let Some(to_hash) = to_entry.tree_hash() {
                    return DiffStep::Descend {
                        from_hash: None,
                        to_hash: Some(to_hash),
                        name: to_entry.name().to_owned(),
                    };
                }
                return DiffStep::Emit(FileChange::new(
                    child_path(&frame.prefix, to_entry.name()),
                    DiffKind::Added,
                ));
            }
            (None, None) => return DiffStep::Done,
        }
    }
}

/// Collect all file changes between two trees.
///
/// This is the materializing variant: it walks the trees via
/// [`diff_trees_visit`] and collects every [`FileChange`] into a
/// [`FileChangeSet`]. Streaming or early-exit consumers should prefer
/// [`diff_trees_visit`], which avoids allocating the full change list.
pub fn diff_trees<S: ObjectSource + ?Sized>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
) -> Result<FileChangeSet, anyhow::Error> {
    let mut changes = FileChangeSet::new();
    // The visitor never short-circuits here, so the `ControlFlow` result is
    // always `Continue(())`; we ignore it and return the collected set.
    let _ = diff_trees_visit(store, from, to, |change| {
        changes.push(change);
        ControlFlow::<()>::Continue(())
    })?;
    Ok(changes)
}

/// Diff two trees with internal iteration, invoking `visitor` for each
/// [`FileChange`] in traversal order.
///
/// This is the streaming counterpart to [`diff_trees`]. The visitor returns a
/// [`ControlFlow`]: `Continue(())` keeps walking, while `Break(value)` stops
/// the traversal immediately — no further subtrees are loaded and no further
/// changes are produced. Early-exit consumers (e.g. "does anything under path
/// X differ?", first-N, quick-status checks) use this to avoid materializing
/// the entire change list.
///
/// On early exit the carried `B` is returned as `Ok(ControlFlow::Break(b))`;
/// on full completion it returns `Ok(ControlFlow::Continue(()))`. Changes are
/// emitted in exactly the same order as [`diff_trees`] collects them, so the
/// two paths are behavior-identical.
pub fn diff_trees_visit<S, V, B>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
    mut visitor: V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: ObjectSource + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B>,
{
    if from == to {
        return Ok(ControlFlow::Continue(()));
    }
    let from_tree = store.get_tree(from)?;
    let to_tree = store.get_tree(to)?;
    let mut stack = vec![DiffFrame {
        from: from_tree,
        to: to_tree,
        prefix: String::new(),
        from_index: 0,
        to_index: 0,
    }];

    while !stack.is_empty() {
        match advance_merge(stack.last_mut().expect("stack is not empty")) {
            DiffStep::Emit(change) => {
                if let ControlFlow::Break(b) = visitor(change) {
                    return Ok(ControlFlow::Break(b));
                }
            }
            DiffStep::Descend {
                from_hash,
                to_hash,
                name,
            } => {
                let from_subtree = from_hash
                    .map(|hash| store.get_tree(&hash))
                    .transpose()?
                    .flatten();
                let to_subtree = to_hash
                    .map(|hash| store.get_tree(&hash))
                    .transpose()?
                    .flatten();
                let prefix = child_path(
                    &stack.last().expect("parent frame remains on stack").prefix,
                    &name,
                );
                stack.push(DiffFrame {
                    from: from_subtree,
                    to: to_subtree,
                    prefix,
                    from_index: 0,
                    to_index: 0,
                });
            }
            DiffStep::Done => {
                stack.pop();
            }
        }
    }

    Ok(ControlFlow::Continue(()))
}

#[cfg(feature = "async-source")]
pub async fn diff_trees_visit_async<S, V, B>(
    store: &S,
    from: &crate::object::ContentHash,
    to: &crate::object::ContentHash,
    mut visitor: V,
) -> Result<ControlFlow<B>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
    V: FnMut(FileChange) -> ControlFlow<B> + Send,
    B: Send,
{
    if from == to {
        return Ok(ControlFlow::Continue(()));
    }
    let from_tree = store.get_tree(from).await?;
    let to_tree = store.get_tree(to).await?;
    let mut stack = vec![DiffFrame {
        from: from_tree,
        to: to_tree,
        prefix: String::new(),
        from_index: 0,
        to_index: 0,
    }];

    while !stack.is_empty() {
        match advance_merge(stack.last_mut().expect("stack is not empty")) {
            DiffStep::Emit(change) => {
                if let ControlFlow::Break(b) = visitor(change) {
                    return Ok(ControlFlow::Break(b));
                }
            }
            DiffStep::Descend {
                from_hash,
                to_hash,
                name,
            } => {
                let from_subtree = match from_hash {
                    Some(hash) => store.get_tree(&hash).await?,
                    None => None,
                };
                let to_subtree = match to_hash {
                    Some(hash) => store.get_tree(&hash).await?,
                    None => None,
                };
                let prefix = child_path(
                    &stack.last().expect("parent frame remains on stack").prefix,
                    &name,
                );
                stack.push(DiffFrame {
                    from: from_subtree,
                    to: to_subtree,
                    prefix,
                    from_index: 0,
                    to_index: 0,
                });
            }
            DiffStep::Done => {
                stack.pop();
            }
        }
    }

    Ok(ControlFlow::Continue(()))
}

fn child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        let mut path = String::with_capacity(prefix.len() + 1 + name.len());
        path.push_str(prefix);
        path.push('/');
        path.push_str(name);
        path
    }
}
