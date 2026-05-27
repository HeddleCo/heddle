// SPDX-License-Identifier: Apache-2.0
//! File rename detection.

use super::analysis_similarity::{SimilarityMethod, compute_similarity_with_language};
use crate::parser::Language;

/// Detect file renames by comparing deleted and added files.
///
/// Returns pairs of (from_path, to_path) for files that appear to be renames.
pub fn detect_file_renames(
    deleted_files: &[(std::path::PathBuf, String)],
    added_files: &[(std::path::PathBuf, String)],
    threshold: f64,
    method: SimilarityMethod,
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    let mut candidates = Vec::new();

    for (deleted_index, (deleted_path, deleted_content)) in deleted_files.iter().enumerate() {
        let deleted_language = Language::from_path(deleted_path);
        for (added_index, (added_path, added_content)) in added_files.iter().enumerate() {
            let added_language = Language::from_path(added_path);
            if deleted_language != Language::Unknown
                && added_language != Language::Unknown
                && deleted_language != added_language
            {
                continue;
            }

            let similarity_language = if deleted_language != Language::Unknown {
                deleted_language
            } else {
                added_language
            };

            let similarity = compute_similarity_with_language(
                deleted_content,
                added_content,
                method,
                similarity_language,
            );

            if similarity >= threshold {
                candidates.push((deleted_index, added_index, similarity));
            }
        }
    }

    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut used_deleted = vec![false; deleted_files.len()];
    let mut used_added = vec![false; added_files.len()];
    let mut renames = Vec::new();

    for (deleted_index, added_index, _) in candidates {
        if used_deleted[deleted_index] || used_added[added_index] {
            continue;
        }

        used_deleted[deleted_index] = true;
        used_added[added_index] = true;
        renames.push((
            deleted_files[deleted_index].0.clone(),
            added_files[added_index].0.clone(),
        ));
    }

    renames
}
