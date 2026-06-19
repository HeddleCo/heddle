// SPDX-License-Identifier: Apache-2.0
use std::path::Path;

use objects::{
    object::{Blob, ContentHash, FileProvenance},
    store::BlockingObjectStore,
};

use super::{HeddleError, Repository, Result, helpers::split_path};

impl Repository {
    pub(super) fn put_file_provenance(&self, provenance: &FileProvenance) -> Result<ContentHash> {
        let bytes = rmp_serde::to_vec(provenance)
            .map_err(|error| HeddleError::InvalidObject(format!("encode provenance: {error}")))?;
        self.store.put_blob(&Blob::new(bytes))
    }

    pub(super) fn lookup_tree_leaf(
        &self,
        root: &ContentHash,
        path: &Path,
    ) -> Result<Option<ContentHash>> {
        let Some((name, rest)) = split_path(path) else {
            return Ok(None);
        };
        let Some(tree) = self.store.get_tree(root)? else {
            return Ok(None);
        };
        let Some(entry) = tree.get(name) else {
            return Ok(None);
        };
        if rest.as_os_str().is_empty() {
            return Ok(entry.is_blob().then_some(entry.hash));
        }
        if !entry.is_tree() {
            return Ok(None);
        }
        self.lookup_tree_leaf(&entry.hash, rest)
    }
}
