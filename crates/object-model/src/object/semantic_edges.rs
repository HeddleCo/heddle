// SPDX-License-Identifier: Apache-2.0
//! State-scoped resolved symbol edges stored as deltas over the first parent.

use serde::{Deserialize, Serialize};

use super::{ContentHash, SemanticIndexError};

/// The relationship represented by a resolved source occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticEdgeKind {
    /// A non-call value reference.
    RefersTo,
    /// A function or method call.
    Calls,
    /// A type-position reference.
    TypeRef,
}

/// A resolved occurrence-to-definition edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResolvedSemanticEdge {
    /// Source-local occurrence id in the source file's semantic node.
    pub source_occurrence: u32,
    /// Repository-relative path containing the target definition.
    pub target_path: String,
    /// Content address of the target file's semantic node.
    pub target_file_node: ContentHash,
    /// Canonical index of the target definition in `SemanticFileNode::symbols`.
    pub target_definition: u32,
    /// Semantic relationship carried by this edge.
    pub kind: SemanticEdgeKind,
}

/// Complete replacement edges for one source file in a binding delta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBindingDelta {
    /// Repository-relative source path.
    pub path: String,
    /// Current semantic file node, or `None` when this record removes a file.
    pub file_node: Option<ContentHash>,
    /// Complete replacement edge list for `path`, sorted canonically.
    pub replace_edges: Vec<ResolvedSemanticEdge>,
}

impl FileBindingDelta {
    /// Construct a canonical replacement record.
    pub fn new(
        path: impl Into<String>,
        file_node: Option<ContentHash>,
        mut replace_edges: Vec<ResolvedSemanticEdge>,
    ) -> Self {
        replace_edges.sort();
        replace_edges.dedup();
        Self {
            path: path.into(),
            file_node,
            replace_edges,
        }
    }
}

/// Content-addressed state binding delta.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingDelta {
    pub format_version: u8,
    /// Content address of the first parent's binding delta.
    pub parent: Option<ContentHash>,
    /// Replacement records for the invalidation frontier, sorted by path.
    pub files: Vec<FileBindingDelta>,
}

impl BindingDelta {
    pub const FORMAT_VERSION: u8 = 1;

    /// Construct a canonical binding delta.
    pub fn new(parent: Option<ContentHash>, mut files: Vec<FileBindingDelta>) -> Self {
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            format_version: Self::FORMAT_VERSION,
            parent,
            files,
        }
    }

    /// Encode this delta as named MessagePack.
    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    /// Decode and version-check a binding delta.
    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let delta: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
        if delta.format_version != Self::FORMAT_VERSION {
            return Err(SemanticIndexError::UnsupportedVersion(delta.format_version));
        }
        Ok(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u8) -> ContentHash {
        ContentHash::from_bytes([seed; 32])
    }

    #[test]
    fn binding_delta_roundtrips_and_canonicalizes() {
        let edge = ResolvedSemanticEdge {
            source_occurrence: 3,
            target_path: "api.rs".to_string(),
            target_file_node: hash(2),
            target_definition: 1,
            kind: SemanticEdgeKind::Calls,
        };
        let delta = BindingDelta::new(
            Some(hash(9)),
            vec![
                FileBindingDelta::new("z.rs", Some(hash(1)), vec![edge.clone(), edge]),
                FileBindingDelta::new("a.rs", None, Vec::new()),
            ],
        );

        assert_eq!(delta.files[0].path, "a.rs");
        assert_eq!(delta.files[1].replace_edges.len(), 1);
        assert_eq!(
            BindingDelta::decode(&delta.encode().unwrap()).unwrap(),
            delta
        );
    }
}
