// SPDX-License-Identifier: Apache-2.0
//! Tree entry definitions.

use serde::{Deserialize, Serialize};

use super::{ContentHash, EntryType, FileMode, TreeError};

/// Validates that a tree entry name is valid. Exposed publicly so
/// callers that build entries at higher layers (the FUSE mount's
/// write-side ops in particular) can fail-fast with the same reject
/// set the tree serializer enforces — otherwise the overlay accepts
/// a name that later blows up at capture with an "invalid object"
/// error rather than a clean EINVAL at write time. Codex heddle#180
/// r13 thread 3293733163 (P2).
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

/// A single entry in a tree.
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
