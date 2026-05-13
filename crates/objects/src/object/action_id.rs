// SPDX-License-Identifier: Apache-2.0
//! Action identifier.

use serde::{Deserialize, Serialize};

use super::ContentHash;

/// Unique identifier for an action (derived from content).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionId(ContentHash);

impl ActionId {
    /// Create from a content hash.
    pub fn from_hash(hash: ContentHash) -> Self {
        Self(hash)
    }

    /// Get the underlying hash.
    pub fn as_hash(&self) -> &ContentHash {
        &self.0
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.short())
    }
}