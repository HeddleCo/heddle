// SPDX-License-Identifier: Apache-2.0
//! Tree types: entries, structure, and supporting enums.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::ContentHash;

// ── TreeError ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    InvalidName(String),
    InvalidStructure(String),
}

impl std::error::Error for TreeError {}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TreeError::InvalidName(msg) => write!(f, "invalid tree entry name: {}", msg),
            TreeError::InvalidStructure(msg) => write!(f, "invalid tree structure: {}", msg),
        }
    }
}

// ── FileMode ────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMode {
    Normal,
    Executable,
    Symlink,
}

impl FileMode {
    pub fn to_byte(&self) -> u8 {
        match self {
            FileMode::Normal => 0,
            FileMode::Executable => 1,
            FileMode::Symlink => 2,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(FileMode::Normal),
            1 => Some(FileMode::Executable),
            2 => Some(FileMode::Symlink),
            _ => None,
        }
    }

    pub fn to_unix_mode(&self) -> u32 {
        match self {
            FileMode::Normal => 0o100644,
            FileMode::Executable => 0o100755,
            FileMode::Symlink => 0o120000,
        }
    }
}

// ── EntryType ───────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryType {
    Blob,
    Tree,
    Symlink,
}

impl EntryType {
    pub fn to_byte(&self) -> u8 {
        match self {
            EntryType::Blob => 0,
            EntryType::Tree => 1,
            EntryType::Symlink => 2,
        }
    }

    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(EntryType::Blob),
            1 => Some(EntryType::Tree),
            2 => Some(EntryType::Symlink),
            _ => None,
        }
    }
}

// ── TreeEntry ───────────────────────────────────────────────────────

pub fn validate_name(name: &str) -> Result<(), TreeError> {
    if name.is_empty() {
        return Err(TreeError::InvalidName("entry name cannot be empty".into()));
    }
    if name == "." || name == ".." {
        return Err(TreeError::InvalidName(format!(
            "'{}' is not a valid entry name",
            name
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(TreeError::InvalidName(
            "entry name cannot contain path separators".into(),
        ));
    }
    if name.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(TreeError::InvalidName(
            "entry name contains control characters".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub mode: FileMode,
    pub entry_type: EntryType,
    pub hash: ContentHash,
}

impl TreeEntry {
    pub(crate) fn validate(&self) -> Result<(), TreeError> {
        validate_name(&self.name)
    }

    pub fn file(
        name: impl Into<String>,
        hash: ContentHash,
        executable: bool,
    ) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            mode: if executable {
                FileMode::Executable
            } else {
                FileMode::Normal
            },
            entry_type: EntryType::Blob,
            hash,
        })
    }

    pub fn directory(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            mode: FileMode::Normal,
            entry_type: EntryType::Tree,
            hash,
        })
    }

    pub fn symlink(name: impl Into<String>, hash: ContentHash) -> Result<Self, TreeError> {
        let name = name.into();
        validate_name(&name)?;
        Ok(Self {
            name,
            mode: FileMode::Symlink,
            entry_type: EntryType::Symlink,
            hash,
        })
    }

    pub fn is_tree(&self) -> bool {
        self.entry_type == EntryType::Tree
    }

    pub fn is_blob(&self) -> bool {
        self.entry_type == EntryType::Blob
    }

    pub fn is_executable(&self) -> bool {
        self.mode == FileMode::Executable
    }

    pub(crate) fn encoded_len(&self) -> usize {
        1 + 1 + self.hash.as_bytes().len() + self.name.len() + 1
    }

    pub(crate) fn update_hasher(&self, hasher: &mut blake3::Hasher) {
        hasher.update(&[self.mode.to_byte()]);
        hasher.update(&[self.entry_type.to_byte()]);
        hasher.update(self.hash.as_bytes());
        hasher.update(self.name.as_bytes());
        hasher.update(&[0]);
    }
}

// ── Tree ────────────────────────────────────────────────────────────

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
