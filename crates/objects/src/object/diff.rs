// SPDX-License-Identifier: Apache-2.0
//! Shared file change types for tree diffing.
//!
//! This module provides common types used across the codebase for representing
//! file-level changes between two trees or worktree states.

use std::fmt;

/// Kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffKind {
    /// File was added.
    Added,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File is unchanged (used in some comparison contexts).
    #[default]
    Unchanged,
}

impl DiffKind {
    /// Returns true if this kind represents an actual change.
    pub fn is_change(&self) -> bool {
        !matches!(self, DiffKind::Unchanged)
    }
}

impl fmt::Display for DiffKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiffKind::Added => write!(f, "added"),
            DiffKind::Modified => write!(f, "modified"),
            DiffKind::Deleted => write!(f, "deleted"),
            DiffKind::Unchanged => write!(f, "unchanged"),
        }
    }
}

/// A single file change with path and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path to the file (relative to repository root).
    pub path: String,
    /// Kind of change.
    pub kind: DiffKind,
}

impl FileChange {
    /// Create a new file change.
    pub fn new(path: impl Into<String>, kind: DiffKind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }

    /// Create an added file change.
    pub fn added(path: impl Into<String>) -> Self {
        Self::new(path, DiffKind::Added)
    }

    /// Create a modified file change.
    pub fn modified(path: impl Into<String>) -> Self {
        Self::new(path, DiffKind::Modified)
    }

    /// Create a deleted file change.
    pub fn deleted(path: impl Into<String>) -> Self {
        Self::new(path, DiffKind::Deleted)
    }

    /// Convert to tuple representation.
    pub fn into_tuple(self) -> (String, DiffKind) {
        (self.path, self.kind)
    }

    /// Convert from tuple representation.
    pub fn from_tuple((path, kind): (String, DiffKind)) -> Self {
        Self { path, kind }
    }
}

impl From<(String, DiffKind)> for FileChange {
    fn from(tuple: (String, DiffKind)) -> Self {
        Self::from_tuple(tuple)
    }
}

impl From<FileChange> for (String, DiffKind) {
    fn from(change: FileChange) -> Self {
        change.into_tuple()
    }
}

/// A collection of file changes with convenience accessors.
#[derive(Debug, Clone, Default)]
pub struct FileChangeSet {
    changes: Vec<FileChange>,
}

impl FileChangeSet {
    /// Create a new empty file change set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new file change set with capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            changes: Vec::with_capacity(capacity),
        }
    }

    /// Add a file change.
    pub fn push(&mut self, change: FileChange) {
        self.changes.push(change);
    }

    /// Add an added file change.
    pub fn push_added(&mut self, path: impl Into<String>) {
        self.changes.push(FileChange::added(path));
    }

    /// Add a modified file change.
    pub fn push_modified(&mut self, path: impl Into<String>) {
        self.changes.push(FileChange::modified(path));
    }

    /// Add a deleted file change.
    pub fn push_deleted(&mut self, path: impl Into<String>) {
        self.changes.push(FileChange::deleted(path));
    }

    /// Returns true if there are no changes.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Returns the number of changes.
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Returns an iterator over the changes.
    pub fn iter(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter()
    }

    /// Returns a mutable iterator over the changes.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut FileChange> {
        self.changes.iter_mut()
    }

    /// Consume and return the underlying vector.
    pub fn into_vec(self) -> Vec<FileChange> {
        self.changes
    }

    /// Get only added files.
    pub fn added(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind == DiffKind::Added)
    }

    /// Get only modified files.
    pub fn modified(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind == DiffKind::Modified)
    }

    /// Get only deleted files.
    pub fn deleted(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind == DiffKind::Deleted)
    }

    /// Returns true if there are no changes.
    pub fn is_clean(&self) -> bool {
        self.changes.is_empty()
    }

    /// Returns the number of added files.
    pub fn added_count(&self) -> usize {
        self.added().count()
    }

    /// Returns the number of modified files.
    pub fn modified_count(&self) -> usize {
        self.modified().count()
    }

    /// Returns the number of deleted files.
    pub fn deleted_count(&self) -> usize {
        self.deleted().count()
    }
}

impl Extend<FileChange> for FileChangeSet {
    fn extend<T: IntoIterator<Item = FileChange>>(&mut self, iter: T) {
        self.changes.extend(iter);
    }
}

impl From<Vec<FileChange>> for FileChangeSet {
    fn from(changes: Vec<FileChange>) -> Self {
        Self { changes }
    }
}

impl From<Vec<(String, DiffKind)>> for FileChangeSet {
    fn from(changes: Vec<(String, DiffKind)>) -> Self {
        Self {
            changes: changes.into_iter().map(FileChange::from_tuple).collect(),
        }
    }
}

impl IntoIterator for FileChangeSet {
    type Item = FileChange;
    type IntoIter = std::vec::IntoIter<FileChange>;

    fn into_iter(self) -> Self::IntoIter {
        self.changes.into_iter()
    }
}

impl<'a> IntoIterator for &'a FileChangeSet {
    type Item = &'a FileChange;
    type IntoIter = std::slice::Iter<'a, FileChange>;

    fn into_iter(self) -> Self::IntoIter {
        self.changes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_change_creation() {
        let change = FileChange::added("src/main.rs");
        assert_eq!(change.path, "src/main.rs");
        assert_eq!(change.kind, DiffKind::Added);

        let change = FileChange::modified("src/lib.rs");
        assert_eq!(change.kind, DiffKind::Modified);

        let change = FileChange::deleted("old.txt");
        assert_eq!(change.kind, DiffKind::Deleted);
    }

    #[test]
    fn test_file_change_tuple_conversion() {
        let change = FileChange::added("foo.txt");
        let tuple: (String, DiffKind) = change.into();
        assert_eq!(tuple, (String::from("foo.txt"), DiffKind::Added));

        let change: FileChange = (String::from("bar.txt"), DiffKind::Modified).into();
        assert_eq!(change.path, "bar.txt");
        assert_eq!(change.kind, DiffKind::Modified);
    }

    #[test]
    fn test_file_change_set_basic() {
        let mut set = FileChangeSet::new();
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);

        set.push_added("a.txt");
        set.push_modified("b.txt");
        set.push_deleted("c.txt");

        assert!(!set.is_empty());
        assert_eq!(set.len(), 3);
        assert_eq!(set.added_count(), 1);
        assert_eq!(set.modified_count(), 1);
        assert_eq!(set.deleted_count(), 1);
    }

    #[test]
    fn test_file_change_set_iterators() {
        let mut set = FileChangeSet::new();
        set.push_added("a.txt");
        set.push_modified("b.txt");
        set.push_deleted("c.txt");

        let added: Vec<_> = set.added().map(|c| &c.path).collect();
        assert_eq!(added, vec!["a.txt"]);

        let modified: Vec<_> = set.modified().map(|c| &c.path).collect();
        assert_eq!(modified, vec!["b.txt"]);

        let deleted: Vec<_> = set.deleted().map(|c| &c.path).collect();
        assert_eq!(deleted, vec!["c.txt"]);
    }

    #[test]
    fn test_file_change_set_conversion() {
        let tuples = vec![
            (String::from("a.txt"), DiffKind::Added),
            (String::from("b.txt"), DiffKind::Modified),
        ];
        let set = FileChangeSet::from(tuples);

        assert_eq!(set.len(), 2);
        assert_eq!(set.added_count(), 1);
        assert_eq!(set.modified_count(), 1);
    }

    #[test]
    fn test_diff_kind_display() {
        assert_eq!(DiffKind::Added.to_string(), "added");
        assert_eq!(DiffKind::Modified.to_string(), "modified");
        assert_eq!(DiffKind::Deleted.to_string(), "deleted");
        assert_eq!(DiffKind::Unchanged.to_string(), "unchanged");
    }

    #[test]
    fn test_diff_kind_is_change() {
        assert!(!DiffKind::Unchanged.is_change());
        assert!(DiffKind::Added.is_change());
        assert!(DiffKind::Modified.is_change());
        assert!(DiffKind::Deleted.is_change());
    }
}