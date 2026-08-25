// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use objects::{error::Result, object::ContentHash, store::ObjectStore};
use repo::Repository;

pub(super) fn verify_tree_content(repo: &Repository, root: ContentHash) -> Result<Option<String>> {
    let mut pending = vec![root];
    let mut seen = HashSet::new();
    while let Some(hash) = pending.pop() {
        if !seen.insert(hash) {
            continue;
        }
        let tree = match repo.store().get_tree(&hash) {
            Ok(Some(tree)) => tree,
            Ok(None) => return Ok(Some(format!("tree {} is missing", hash.short()))),
            Err(error) => {
                return Ok(Some(format!(
                    "tree {} failed content binding: {error}",
                    hash.short()
                )));
            }
        };
        if tree.hash() != hash {
            return Ok(Some(format!(
                "tree {} failed content binding",
                hash.short()
            )));
        }
        for entry in tree.entries() {
            if let Some(child) = entry.tree_hash() {
                pending.push(child);
            } else if let Some(blob_hash) = entry.blob_hash() {
                let blob = match repo.store().get_blob(&blob_hash) {
                    Ok(Some(blob)) => blob,
                    Ok(None) => {
                        return Ok(Some(format!(
                            "tree path '{}' references missing blob {}",
                            entry.name(),
                            blob_hash.short()
                        )));
                    }
                    Err(error) => {
                        return Ok(Some(format!(
                            "tree path '{}' blob {} failed content binding: {error}",
                            entry.name(),
                            blob_hash.short()
                        )));
                    }
                };
                if blob.hash() != blob_hash {
                    return Ok(Some(format!(
                        "tree path '{}' blob {} failed content binding",
                        entry.name(),
                        blob_hash.short()
                    )));
                }
            }
        }
    }
    Ok(None)
}
