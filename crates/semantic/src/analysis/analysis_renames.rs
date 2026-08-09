// SPDX-License-Identifier: Apache-2.0
//! File rename detection.

use super::analysis_similarity::{PreparedSimilarity, SimilarityMethod};
use crate::parser::Language;
use merge::RenameCandidateIndex;

/// Detect file renames by comparing deleted and added files.
///
/// Returns pairs of (from_path, to_path) for files that appear to be renames.
pub fn detect_file_renames(
    deleted_files: &[(std::path::PathBuf, String)],
    added_files: &[(std::path::PathBuf, String)],
    threshold: f64,
    method: SimilarityMethod,
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    if deleted_files.is_empty() || added_files.is_empty() {
        return Vec::new();
    }

    let deleted_languages = deleted_files
        .iter()
        .map(|(path, _)| Language::from_path(path))
        .collect::<Vec<_>>();
    let added_languages = added_files
        .iter()
        .map(|(path, _)| Language::from_path(path))
        .collect::<Vec<_>>();
    let deleted_prepared = deleted_files
        .iter()
        .zip(&deleted_languages)
        .map(|((_, content), language)| {
            PreparedSimilarity::new(
                content,
                method,
                preparation_languages(*language, &added_languages),
            )
        })
        .collect::<Vec<_>>();
    let added_prepared = added_files
        .iter()
        .zip(&added_languages)
        .map(|((_, content), language)| {
            PreparedSimilarity::new(
                content,
                method,
                preparation_languages(*language, &deleted_languages),
            )
        })
        .collect::<Vec<_>>();

    let mut candidates = RenameCandidateIndex::new(deleted_files.len(), added_files.len());

    for (deleted_index, deleted) in deleted_prepared.iter().enumerate() {
        let deleted_language = deleted_languages[deleted_index];
        for (added_index, added) in added_prepared.iter().enumerate() {
            let added_language = added_languages[added_index];
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
            let similarity = deleted.similarity(added, similarity_language);

            if similarity >= threshold {
                candidates.push(deleted_index, added_index, similarity);
            }
        }
    }

    candidates
        .assign()
        .into_iter()
        .map(|assignment| {
            (
                deleted_files[assignment.source_index].0.clone(),
                added_files[assignment.target_index].0.clone(),
            )
        })
        .collect()
}

fn preparation_languages(language: Language, counterparts: &[Language]) -> Vec<Language> {
    if language != Language::Unknown {
        return counterparts
            .iter()
            .any(|counterpart| *counterpart == language || *counterpart == Language::Unknown)
            .then_some(language)
            .into_iter()
            .collect();
    }

    let mut languages = Vec::new();
    for counterpart in counterparts.iter().copied() {
        if !languages.contains(&counterpart) {
            languages.push(counterpart);
        }
    }
    languages
}
