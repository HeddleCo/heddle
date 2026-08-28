// SPDX-License-Identifier: Apache-2.0
//! Reverse-dependency frontier: which files must re-resolve after a change.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use objects::object::ReverseDependencyIndex;
use semantic::cross_file_resolution::{FileResolution, RepositorySemanticFile};

/// Walk file → importers from `changed` to the transitive invalidation frontier.
pub(crate) fn invalidation_frontier(
    changed: &BTreeSet<String>,
    index: &ReverseDependencyIndex,
) -> BTreeSet<String> {
    let mut frontier = changed.clone();
    let mut queue = VecDeque::from_iter(changed.iter().cloned());
    while let Some(path) = queue.pop_front() {
        for importer in index.importers_of(&path) {
            if frontier.insert(importer.clone()) {
                queue.push_back(importer.clone());
            }
        }
    }
    frontier
}

/// Replace frontier files' outgoing edges in the reverse-dependency index.
pub(crate) fn patch_importer_index(
    parent: Option<&ReverseDependencyIndex>,
    frontier: &BTreeSet<String>,
    resolutions: &BTreeMap<String, FileResolution>,
    current_files: &BTreeMap<String, RepositorySemanticFile>,
) -> ReverseDependencyIndex {
    let mut importers = parent
        .map(|index| {
            index
                .importers
                .iter()
                .map(|(target, sources)| (target.clone(), sources.iter().cloned().collect()))
                .collect::<BTreeMap<String, BTreeSet<String>>>()
        })
        .unwrap_or_default();

    for sources in importers.values_mut() {
        for path in frontier {
            sources.remove(path);
        }
    }
    for path in frontier {
        if !current_files.contains_key(path) {
            importers.remove(path);
        }
    }
    for (source, resolution) in resolutions {
        for dep in &resolution.dependencies {
            importers
                .entry(dep.clone())
                .or_default()
                .insert(source.clone());
        }
    }
    ReverseDependencyIndex::new(importers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_is_changed_files_plus_transitive_importers() {
        let index = ReverseDependencyIndex::from_dependencies(&BTreeMap::from([
            ("b.rs".to_string(), BTreeSet::from(["a.rs".to_string()])),
            ("c.rs".to_string(), BTreeSet::from(["b.rs".to_string()])),
            ("e.rs".to_string(), BTreeSet::from(["d.rs".to_string()])),
        ]));
        let changed = BTreeSet::from(["a.rs".to_string()]);
        assert_eq!(
            invalidation_frontier(&changed, &index),
            BTreeSet::from(["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()])
        );
    }
}
