// SPDX-License-Identifier: Apache-2.0
//! File-level rename decisions for context annotation anchors.

#![cfg(feature = "tree-sitter-symbols")]

use std::{collections::HashMap, path::PathBuf};

use semantic::analysis::{SimilarityMethod, detect_file_renames};

use crate::discussion_anchor_travel::RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContextFileTravel {
    Present,
    Moved(String),
    Ambiguous(Vec<String>),
    Orphaned,
}

/// Resolve one vanished context target through the same semantic file-rename
/// detector and confidence threshold used by discussion anchor travel.
///
/// The detector's batch API chooses a one-to-one assignment. Context must
/// surface multiple plausible destinations instead, so each added file is
/// checked independently with the shared primitive and the accepted paths are
/// collected before making a decision.
pub(crate) fn context_file_travel(
    old_path: &str,
    old_files: &HashMap<String, Vec<u8>>,
    new_files: &HashMap<String, Vec<u8>>,
) -> ContextFileTravel {
    if new_files.contains_key(old_path) {
        return ContextFileTravel::Present;
    }
    let Some(old_source) = old_files.get(old_path) else {
        return ContextFileTravel::Orphaned;
    };
    let deleted = vec![(
        PathBuf::from(old_path),
        String::from_utf8_lossy(old_source).into_owned(),
    )];
    let mut added_paths: Vec<&String> = new_files
        .keys()
        .filter(|path| !old_files.contains_key(*path))
        .collect();
    added_paths.sort();

    let mut candidates = Vec::new();
    for path in added_paths {
        let Some(source) = new_files.get(path) else {
            continue;
        };
        let added = vec![(
            PathBuf::from(path),
            String::from_utf8_lossy(source).into_owned(),
        )];
        if !detect_file_renames(
            &deleted,
            &added,
            RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL as f64,
            SimilarityMethod::Tokens,
        )
        .is_empty()
        {
            candidates.push(path.clone());
        }
    }

    match candidates.as_slice() {
        [] => ContextFileTravel::Orphaned,
        [path] => ContextFileTravel::Moved(path.clone()),
        _ => ContextFileTravel::Ambiguous(candidates),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> HashMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(path, source)| (path.to_string(), source.as_bytes().to_vec()))
            .collect()
    }

    const SOURCE: &str = "fn guarded() {\n    let value = 1;\n}\n";

    #[test]
    fn unique_candidate_moves() {
        assert_eq!(
            context_file_travel(
                "src/old.rs",
                &files(&[("src/old.rs", SOURCE)]),
                &files(&[("src/new.rs", SOURCE)]),
            ),
            ContextFileTravel::Moved("src/new.rs".to_string())
        );
    }

    #[test]
    fn two_candidates_are_ambiguous() {
        assert_eq!(
            context_file_travel(
                "src/old.rs",
                &files(&[("src/old.rs", SOURCE)]),
                &files(&[("src/a.rs", SOURCE), ("src/b.rs", SOURCE)]),
            ),
            ContextFileTravel::Ambiguous(vec!["src/a.rs".to_string(), "src/b.rs".to_string(),])
        );
    }

    #[test]
    fn delete_is_orphaned() {
        assert_eq!(
            context_file_travel(
                "src/old.rs",
                &files(&[("src/old.rs", SOURCE)]),
                &HashMap::new(),
            ),
            ContextFileTravel::Orphaned
        );
    }
}
