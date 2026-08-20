// SPDX-License-Identifier: Apache-2.0
//! Path lookup through Trees for a single blame path.

use std::path::{Component, Path};

use crate::object::{ContentHash, ObjectSource, Tree};

use super::types::BlameSliceError;

pub(super) fn lookup_blob_at_path<S: ObjectSource>(
    source: &S,
    tree_hash: &ContentHash,
    path: &Path,
) -> Result<Option<ContentHash>, BlameSliceError> {
    let Some(tree) = source.get_tree(tree_hash)? else {
        return Err(BlameSliceError::MissingObject {
            kind: "tree",
            id: tree_hash.to_string(),
        });
    };
    walk_blob(source, tree, path)
}

fn walk_blob<S: ObjectSource>(
    source: &S,
    mut tree: Tree,
    path: &Path,
) -> Result<Option<ContentHash>, BlameSliceError> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return Ok(None);
                };
                segments.push(name.to_string());
            }
            Component::CurDir => {}
            _ => return Ok(None),
        }
    }
    if segments.is_empty() {
        return Ok(None);
    }
    let last = segments.len() - 1;
    for (index, name) in segments.iter().enumerate() {
        let Some(entry) = tree.get(name).cloned() else {
            return Ok(None);
        };
        if index == last {
            return Ok(entry.blob_hash());
        }
        let Some(child_hash) = entry.tree_hash() else {
            return Ok(None);
        };
        tree = source.get_tree(&child_hash)?.ok_or_else(|| {
            BlameSliceError::MissingObject {
                kind: "tree",
                id: child_hash.to_string(),
            }
        })?;
    }
    Ok(None)
}
