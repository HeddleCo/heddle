// SPDX-License-Identifier: Apache-2.0
//! Path lookup through Trees for a single blame path.

use std::path::Path;

use crate::{
    object::{ContentHash, LeafPolicy, ObjectSource, resolve_tree_path},
    store::Result,
};

pub(super) fn lookup_blob_at_path<S: ObjectSource>(
    source: &S,
    tree_hash: &ContentHash,
    path: &Path,
) -> Result<Option<ContentHash>> {
    let Some(tree) = source.get_tree(tree_hash)? else {
        return Ok(None);
    };
    let Some(target) = resolve_tree_path(source, &tree.hash(), path, LeafPolicy::Entry)
        .ok()
        .flatten()
    else {
        return Ok(None);
    };
    Ok(target.entry.blob_hash())
}
