// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use objects::delta::DeltaEncoder;

use super::{RenameMatch, RenameMatcherConfig, RenameMatcherStats};

const WEIGHT_DELTA: f64 = 0.50;
const WEIGHT_SEMANTIC: f64 = 0.30;
const WEIGHT_PATH: f64 = 0.20;
const HIGH_CONFIDENCE_DELTA: f64 = 0.95;

pub(super) fn composite_score(
    from_path: &str,
    to_path: &str,
    from_content: &[u8],
    to_content: &[u8],
    path_score: f64,
    config: RenameMatcherConfig,
    stats: &mut RenameMatcherStats,
) -> f64 {
    let delta_score = delta_similarity(from_content, to_content);
    if delta_score >= HIGH_CONFIDENCE_DELTA {
        stats.high_confidence_delta_pairs += 1;
        return WEIGHT_DELTA * delta_score + WEIGHT_SEMANTIC + WEIGHT_PATH * path_score;
    }

    let semantic_score = if delta_score < 0.3 {
        0.0
    } else {
        stats.semantic_scored_pairs += 1;
        config.semantic_score(from_path, to_path, from_content, to_content)
    };

    if semantic_score == 0.0 {
        let delta_weight = WEIGHT_DELTA + WEIGHT_SEMANTIC;
        return delta_weight * delta_score + WEIGHT_PATH * path_score;
    }

    WEIGHT_DELTA * delta_score + WEIGHT_SEMANTIC * semantic_score + WEIGHT_PATH * path_score
}

pub(super) fn delta_similarity(base: &[u8], target: &[u8]) -> f64 {
    if base.is_empty() && target.is_empty() {
        return 1.0;
    }
    if base.is_empty() || target.is_empty() {
        return 0.0;
    }
    if base == target {
        return 1.0;
    }

    let delta_size = DeltaEncoder::estimate_delta_size(base, target);
    let ratio = delta_size as f64 / target.len() as f64;
    (1.0 - ratio).max(0.0)
}

pub(super) fn path_similarity(left: &str, right: &str) -> f64 {
    let left_parts: Vec<&str> = left.split('/').collect();
    let right_parts: Vec<&str> = right.split('/').collect();
    let shared_suffix = left_parts
        .iter()
        .rev()
        .zip(right_parts.iter().rev())
        .take_while(|(left_part, right_part)| left_part == right_part)
        .count();
    let component_count = left_parts.len().max(right_parts.len());
    let suffix_score = if component_count == 0 {
        0.0
    } else {
        shared_suffix as f64 / component_count as f64
    };
    let same_extension = extension_of(left) == extension_of(right) && extension_of(left).is_some();

    suffix_score * 0.7 + if same_extension { 0.3 } else { 0.0 }
}

/// Infer directory renames from file-level rename matches.
pub fn infer_directory_renames(renames: &HashMap<String, RenameMatch>) -> Vec<(String, String)> {
    let mut directory_targets: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut directory_counts: HashMap<String, usize> = HashMap::new();

    for rename in renames.values() {
        let (Some(from_dir), Some(to_dir)) =
            (parent_dir(&rename.from_path), parent_dir(&rename.to_path))
        else {
            continue;
        };
        *directory_targets
            .entry(from_dir.clone())
            .or_default()
            .entry(to_dir.clone())
            .or_insert(0) += 1;
        *directory_counts.entry(from_dir).or_insert(0) += 1;
    }

    let mut result = Vec::new();
    for (from_dir, targets) in directory_targets {
        let total = directory_counts.get(&from_dir).copied().unwrap_or(0);
        if total < 2 {
            continue;
        }
        for (to_dir, count) in targets {
            if from_dir != to_dir && count as f64 / total as f64 >= 0.8 {
                result.push((from_dir.clone(), to_dir));
            }
        }
    }

    result.sort();
    result
}

pub(super) fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(super) fn file_stem_of(path: &str) -> &str {
    path.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(path)
}

pub(super) fn extension_of(path: &str) -> Option<&str> {
    file_name_of(path)
        .rsplit_once('.')
        .map(|(_, extension)| extension)
}

fn parent_dir(path: &str) -> Option<String> {
    path.rsplit_once('/')
        .map(|(directory, _)| directory.to_string())
}
