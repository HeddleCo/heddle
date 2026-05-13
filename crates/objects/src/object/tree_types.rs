// SPDX-License-Identifier: Apache-2.0
//! Tree entry type definitions.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Error type for tree operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeError {
    /// Invalid entry name.
    InvalidName(String),
    /// Invalid tree structure.
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

/// File mode for tree entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMode {
    /// Regular file (0644)
    Normal,
    /// Executable file (0755)
    Executable,
    /// Symbolic link
    Symlink,
}

impl FileMode {
    /// Convert to a byte for serialization.
    pub fn to_byte(&self) -> u8 {
        match self {
            FileMode::Normal => 0,
            FileMode::Executable => 1,
            FileMode::Symlink => 2,
        }
    }

    /// Parse from a byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(FileMode::Normal),
            1 => Some(FileMode::Executable),
            2 => Some(FileMode::Symlink),
            _ => None,
        }
    }

    /// Convert to Unix mode bits.
    pub fn to_unix_mode(&self) -> u32 {
        match self {
            FileMode::Normal => 0o100644,
            FileMode::Executable => 0o100755,
            FileMode::Symlink => 0o120000,
        }
    }
}

/// Entry type in a tree (blob or subtree).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryType {
    /// A file (blob)
    Blob,
    /// A directory (subtree)
    Tree,
    /// A symlink
    Symlink,
}

impl EntryType {
    /// Convert to a byte for serialization.
    pub fn to_byte(&self) -> u8 {
        match self {
            EntryType::Blob => 0,
            EntryType::Tree => 1,
            EntryType::Symlink => 2,
        }
    }

    /// Parse from a byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(EntryType::Blob),
            1 => Some(EntryType::Tree),
            2 => Some(EntryType::Symlink),
            _ => None,
        }
    }
}