// SPDX-License-Identifier: Apache-2.0
//! File → importers reverse-dependency index for incremental re-resolution.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::SemanticIndexError;

/// Content-addressed file → importers map for one state.
///
/// `importers[target]` lists every source file whose resolved dependencies
/// include `target`. Capture walks this map from changed files to obtain the
/// invalidation frontier without re-resolving the rest of the repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReverseDependencyIndex {
    pub format_version: u8,
    /// Imported file → sorted importer paths.
    pub importers: BTreeMap<String, Vec<String>>,
}

impl ReverseDependencyIndex {
    pub const FORMAT_VERSION: u8 = 1;

    /// Construct a canonical index from an importer map.
    pub fn new(importers: BTreeMap<String, BTreeSet<String>>) -> Self {
        let importers = importers
            .into_iter()
            .filter_map(|(target, sources)| {
                if sources.is_empty() {
                    None
                } else {
                    Some((target, sources.into_iter().collect()))
                }
            })
            .collect();
        Self {
            format_version: Self::FORMAT_VERSION,
            importers,
        }
    }

    /// Invert source → dependencies into a file → importers index.
    pub fn from_dependencies(dependencies: &BTreeMap<String, BTreeSet<String>>) -> Self {
        let mut importers = BTreeMap::<String, BTreeSet<String>>::new();
        for (source, deps) in dependencies {
            for dep in deps {
                importers
                    .entry(dep.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
        Self::new(importers)
    }

    /// Look up the files that import `path`.
    pub fn importers_of(&self, path: &str) -> &[String] {
        self.importers
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Encode this index as named MessagePack.
    pub fn encode(&self) -> Result<Vec<u8>, SemanticIndexError> {
        rmp_serde::to_vec_named(self).map_err(|err| SemanticIndexError::Encoding(err.to_string()))
    }

    /// Decode and version-check a reverse-dependency index.
    pub fn decode(bytes: &[u8]) -> Result<Self, SemanticIndexError> {
        let index: Self = rmp_serde::from_slice(bytes)
            .map_err(|err| SemanticIndexError::Encoding(err.to_string()))?;
        if index.format_version != Self::FORMAT_VERSION {
            return Err(SemanticIndexError::UnsupportedVersion(index.format_version));
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_roundtrips_and_drops_empty_importer_sets() {
        let index = ReverseDependencyIndex::from_dependencies(&BTreeMap::from([
            (
                "b.rs".to_string(),
                BTreeSet::from(["a.rs".to_string(), "a.rs".to_string()]),
            ),
            ("c.rs".to_string(), BTreeSet::from(["b.rs".to_string()])),
            ("d.rs".to_string(), BTreeSet::new()),
        ]));

        assert_eq!(index.importers_of("a.rs"), ["b.rs"]);
        assert_eq!(index.importers_of("b.rs"), ["c.rs"]);
        assert!(index.importers_of("d.rs").is_empty());
        assert_eq!(
            ReverseDependencyIndex::decode(&index.encode().unwrap()).unwrap(),
            index
        );
    }
}
