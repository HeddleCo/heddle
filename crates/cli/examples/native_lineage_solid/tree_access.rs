// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::{Context, Result};
use objects::{
    object::{ContentHash, Tree},
    store::{FsStore, ObjectStore},
};

pub fn leaf_hash(store: &FsStore, root: ContentHash, path: &str) -> Result<Option<ContentHash>> {
    let mut tree = get_tree(store, root)?;
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str().to_string_lossy();
        let Some(entry) = tree.get(&name) else {
            return Ok(None);
        };
        if components.peek().is_none() {
            return Ok(entry.leaf_content_hash());
        }
        let Some(hash) = entry.tree_hash() else {
            return Ok(None);
        };
        tree = get_tree(store, hash)?;
    }
    Ok(None)
}

pub fn leaf_paths(store: &FsStore, root: ContentHash) -> Result<Vec<(String, ContentHash)>> {
    let mut output = Vec::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((prefix, hash)) = stack.pop() {
        for entry in get_tree(store, hash)?.entries().iter().rev() {
            let path = if prefix.is_empty() {
                entry.name().to_string()
            } else {
                format!("{prefix}/{}", entry.name())
            };
            if let Some(hash) = entry.tree_hash() {
                stack.push((path, hash));
            } else if let Some(hash) = entry.leaf_content_hash() {
                output.push((path, hash));
            }
        }
    }
    Ok(output)
}

pub fn get_tree(store: &FsStore, hash: ContentHash) -> Result<Tree> {
    store
        .get_tree(&hash)?
        .with_context(|| format!("missing tree {hash}"))
}
