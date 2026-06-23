// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::Result;
use objects::{
    object::{ContentHash, EntryType, Tree},
    store::ObjectStore,
};
use tracing::debug;

use super::{
    RenameDetection, RenameMatch, RenameMatcherConfig, RenameMatcherStats,
    candidates::{build_added_index, collect_candidate_positions, load_candidate_files},
    scoring::{composite_score, path_similarity},
};

const CANDIDATE_CAP: usize = 10_000;

/// Flatten a tree into `path -> (hash, entry_type)` entries.
pub fn flatten_tree(
    store: &impl ObjectStore,
    tree: &Tree,
    prefix: &str,
) -> Result<HashMap<String, (ContentHash, EntryType)>> {
    let mut result = HashMap::new();

    for entry in tree.entries() {
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };

        match entry.entry_type {
            EntryType::Blob | EntryType::Symlink => {
                result.insert(path, (entry.hash, entry.entry_type));
            }
            EntryType::Tree => {
                if let Some(subtree) = store.get_tree(&entry.hash)? {
                    result.extend(flatten_tree(store, &subtree, &path)?);
                }
            }
        }
    }

    Ok(result)
}

/// Detect file renames between a base tree and a branch tree.
pub fn detect_renames(
    store: &impl ObjectStore,
    base: &HashMap<String, (ContentHash, EntryType)>,
    branch: &HashMap<String, (ContentHash, EntryType)>,
    config: RenameMatcherConfig,
) -> Result<HashMap<String, RenameMatch>> {
    Ok(detect_renames_with_stats(store, base, branch, config)?.matches)
}

/// Detect file renames and return diagnostic matcher stats.
pub fn detect_renames_with_stats(
    store: &impl ObjectStore,
    base: &HashMap<String, (ContentHash, EntryType)>,
    branch: &HashMap<String, (ContentHash, EntryType)>,
    config: RenameMatcherConfig,
) -> Result<RenameDetection> {
    let deleted: Vec<(&String, &ContentHash)> = base
        .iter()
        .filter(|(path, (_, et))| *et == EntryType::Blob && !branch.contains_key(*path))
        .map(|(path, (hash, _))| (path, hash))
        .collect();
    let added: Vec<(&String, &ContentHash)> = branch
        .iter()
        .filter(|(path, (_, et))| *et == EntryType::Blob && !base.contains_key(*path))
        .map(|(path, (hash, _))| (path, hash))
        .collect();

    let mut stats = RenameMatcherStats {
        deleted_files: deleted.len(),
        added_files: added.len(),
        total_possible_pairs: deleted.len().saturating_mul(added.len()),
        ..RenameMatcherStats::default()
    };

    if deleted.is_empty() || added.is_empty() {
        trace_matcher_stats(&stats);
        return Ok(RenameDetection {
            matches: HashMap::new(),
            stats,
        });
    }

    let mut matches = HashMap::new();
    let mut used_deleted = vec![false; deleted.len()];
    let mut used_added = vec![false; added.len()];

    match_exact_hashes(
        &deleted,
        &added,
        &mut used_deleted,
        &mut used_added,
        &mut matches,
        &mut stats,
    );

    let remaining_deleted: Vec<(usize, &str, &ContentHash)> = deleted
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_deleted[*index])
        .map(|(index, (path, hash))| (index, path.as_str(), *hash))
        .collect();
    let remaining_added: Vec<(usize, &str, &ContentHash)> = added
        .iter()
        .enumerate()
        .filter(|(index, _)| !used_added[*index])
        .map(|(index, (path, hash))| (index, path.as_str(), *hash))
        .collect();

    if remaining_deleted.is_empty() || remaining_added.is_empty() {
        stats.matched_pairs = matches.len();
        trace_matcher_stats(&stats);
        return Ok(RenameDetection { matches, stats });
    }

    let use_content = remaining_deleted
        .len()
        .saturating_mul(remaining_added.len())
        <= CANDIDATE_CAP;
    stats.used_content = use_content;

    let deleted_files = load_candidate_files(store, &remaining_deleted, use_content, &mut stats)?;
    let added_index = build_added_index(store, &remaining_added, use_content, &mut stats)?;
    let mut candidates = Vec::new();

    for deleted_file in &deleted_files {
        for added_position in collect_candidate_positions(deleted_file, &added_index) {
            let added_file = &added_index.files[added_position];
            if used_added[added_file.index] {
                continue;
            }

            stats.metadata_candidate_pairs += 1;
            let path_score = path_similarity(deleted_file.path.path, added_file.path.path);
            let score = if !use_content {
                path_score
            } else {
                match (&deleted_file.content, &added_file.content) {
                    (Some(from_content), Some(to_content)) => composite_score(
                        deleted_file.path.path,
                        added_file.path.path,
                        from_content,
                        to_content,
                        path_score,
                        config,
                        &mut stats,
                    ),
                    _ => path_score * 0.20,
                }
            };

            stats.scored_pairs += 1;
            if score >= config.threshold {
                stats.threshold_matches += 1;
                candidates.push((deleted_file.index, added_file.index, score));
            }
        }
    }

    candidates.sort_by(|left, right| {
        right
            .2
            .partial_cmp(&left.2)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for (deleted_index, added_index, score) in candidates {
        if used_deleted[deleted_index] || used_added[added_index] {
            continue;
        }

        used_deleted[deleted_index] = true;
        used_added[added_index] = true;

        let deleted_path = deleted[deleted_index].0;
        let added_path = added[added_index].0;
        matches.insert(
            deleted_path.clone(),
            RenameMatch {
                from_path: deleted_path.clone(),
                to_path: added_path.clone(),
                score,
                from_hash: *deleted[deleted_index].1,
                to_hash: *added[added_index].1,
            },
        );
    }

    stats.matched_pairs = matches.len();
    trace_matcher_stats(&stats);
    Ok(RenameDetection { matches, stats })
}

fn trace_matcher_stats(stats: &RenameMatcherStats) {
    debug!(
        deleted_files = stats.deleted_files,
        added_files = stats.added_files,
        exact_hash_matches = stats.exact_hash_matches,
        total_possible_pairs = stats.total_possible_pairs,
        metadata_candidate_pairs = stats.metadata_candidate_pairs,
        scored_pairs = stats.scored_pairs,
        semantic_scored_pairs = stats.semantic_scored_pairs,
        high_confidence_delta_pairs = stats.high_confidence_delta_pairs,
        threshold_matches = stats.threshold_matches,
        matched_pairs = stats.matched_pairs,
        blob_loads = stats.blob_loads,
        blob_bytes_loaded = stats.blob_bytes_loaded,
        used_content = stats.used_content,
        "rename matcher completed"
    );
}

fn match_exact_hashes(
    deleted: &[(&String, &ContentHash)],
    added: &[(&String, &ContentHash)],
    used_deleted: &mut [bool],
    used_added: &mut [bool],
    matches: &mut HashMap<String, RenameMatch>,
    stats: &mut RenameMatcherStats,
) {
    let mut added_by_hash: HashMap<&ContentHash, Vec<usize>> = HashMap::new();
    for (index, (_, hash)) in added.iter().enumerate() {
        added_by_hash.entry(hash).or_default().push(index);
    }

    for (deleted_index, (deleted_path, deleted_hash)) in deleted.iter().enumerate() {
        let Some(indices) = added_by_hash.get(deleted_hash) else {
            continue;
        };

        let best_added = indices
            .iter()
            .copied()
            .filter(|index| !used_added[*index])
            .max_by(|left, right| {
                let left_score = path_similarity(deleted_path, added[*left].0);
                let right_score = path_similarity(deleted_path, added[*right].0);
                left_score
                    .partial_cmp(&right_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

        let Some(added_index) = best_added else {
            continue;
        };

        used_deleted[deleted_index] = true;
        used_added[added_index] = true;
        stats.exact_hash_matches += 1;
        matches.insert(
            (*deleted_path).clone(),
            RenameMatch {
                from_path: (*deleted_path).clone(),
                to_path: added[added_index].0.clone(),
                score: 1.0,
                from_hash: **deleted_hash,
                to_hash: *added[added_index].1,
            },
        );
    }
}
