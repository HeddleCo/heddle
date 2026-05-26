// SPDX-License-Identifier: Apache-2.0
//! Tree structure.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ContentHash, TreeEntry, TreeError};

/// A tree represents a directory structure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    entries: Vec<TreeEntry>,
}

impl Tree {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn from_entries(mut entries: Vec<TreeEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Self { entries }
    }

    pub fn validate(&self) -> Result<(), TreeError> {
        let mut previous_name: Option<&str> = None;

        for entry in &self.entries {
            entry.validate()?;

            if let Some(previous) = previous_name
                && previous >= entry.name.as_str()
            {
                return Err(TreeError::InvalidStructure(
                    "entries must be strictly sorted by name".to_string(),
                ));
            }

            previous_name = Some(&entry.name);
        }

        Ok(())
    }

    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        let index = self
            .entries
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .ok()?;
        self.entries.get(index)
    }

    pub fn insert(&mut self, entry: TreeEntry) {
        self.entries.retain(|e| e.name != entry.name);

        let pos = self
            .entries
            .iter()
            .position(|e| e.name > entry.name)
            .unwrap_or(self.entries.len());
        self.entries.insert(pos, entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<TreeEntry> {
        let pos = self.entries.iter().position(|e| e.name == name)?;
        Some(self.entries.remove(pos))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn hash(&self) -> ContentHash {
        let total_len: usize = self.entries.iter().map(TreeEntry::encoded_len).sum();
        ContentHash::compute_typed_with_len("tree", total_len as u64, |hasher| {
            for entry in &self.entries {
                entry.update_hasher(hasher);
            }
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &TreeEntry> {
        self.entries.iter()
    }

    pub fn get_path(&self, path: &Path) -> Option<&TreeEntry> {
        let name = path.file_name()?.to_str()?;
        if path.parent().is_none_or(|p| p.as_os_str().is_empty()) {
            self.get(name)
        } else {
            None
        }
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoIterator for Tree {
    type Item = TreeEntry;
    type IntoIter = std::vec::IntoIter<TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a Tree {
    type Item = &'a TreeEntry;
    type IntoIter = std::slice::Iter<'a, TreeEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}
