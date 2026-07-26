// SPDX-License-Identifier: Apache-2.0
//! Blob storage for file contents.

use serde::{Deserialize, Serialize};

use super::ContentHash;

/// A blob stores raw file contents.
///
/// Blobs are content-addressed: the hash is computed from the content itself.
/// Two files with identical content will share the same blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    content: Vec<u8>,
}

impl Blob {
    /// Create a new blob from content.
    pub fn new(content: Vec<u8>) -> Self {
        Self { content }
    }

    /// Create a blob from a byte slice.
    pub fn from_slice(content: &[u8]) -> Self {
        Self {
            content: content.to_vec(),
        }
    }

    /// Get the content.
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Get the content as a string (if valid UTF-8).
    pub fn content_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.content).ok()
    }

    /// Consume the blob and return the content.
    pub fn into_content(self) -> Vec<u8> {
        self.content
    }

    /// Compute the content hash for this blob.
    pub fn hash(&self) -> ContentHash {
        ContentHash::compute_typed("blob", &self.content)
    }

    /// Get the size in bytes.
    pub fn size(&self) -> usize {
        self.content.len()
    }

    /// Check if the blob is empty.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

impl From<Vec<u8>> for Blob {
    fn from(content: Vec<u8>) -> Self {
        Self::new(content)
    }
}

impl From<&[u8]> for Blob {
    fn from(content: &[u8]) -> Self {
        Self::from_slice(content)
    }
}

impl From<String> for Blob {
    fn from(content: String) -> Self {
        Self::new(content.into_bytes())
    }
}

impl From<&str> for Blob {
    fn from(content: &str) -> Self {
        Self::new(content.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blob_creation() {
        let blob = Blob::new(b"hello world".to_vec());
        assert_eq!(blob.content(), b"hello world");
        assert_eq!(blob.size(), 11);
        assert!(!blob.is_empty());
    }

    #[test]
    fn test_blob_hash_deterministic() {
        let blob1 = Blob::from("hello");
        let blob2 = Blob::from("hello");
        assert_eq!(blob1.hash(), blob2.hash());
    }

    #[test]
    fn test_blob_hash_differs_for_different_content() {
        let blob1 = Blob::from("hello");
        let blob2 = Blob::from("world");
        assert_ne!(blob1.hash(), blob2.hash());
    }

    #[test]
    fn test_blob_content_str() {
        let blob = Blob::from("hello");
        assert_eq!(blob.content_str(), Some("hello"));

        let binary_blob = Blob::new(vec![0xff, 0xfe]);
        assert_eq!(binary_blob.content_str(), None);
    }

    #[test]
    fn test_empty_blob() {
        let blob = Blob::new(vec![]);
        assert!(blob.is_empty());
        assert_eq!(blob.size(), 0);
    }
}
