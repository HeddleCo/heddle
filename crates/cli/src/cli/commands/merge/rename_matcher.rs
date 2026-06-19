// SPDX-License-Identifier: Apache-2.0
//! Reusable rename-matching primitives for merge planning.

use std::collections::HashMap;

use anyhow::Result;
use objects::{
    delta::DeltaEncoder,
    object::{ContentHash, EntryType, Tree},
    store::BlockingObjectStore,
};
use tracing::debug;

#[derive(Debug, Clone)]
pub(crate) struct RenameMatch {
    pub from_path: String,
    pub to_path: String,
    pub score: f64,
    pub from_hash: ContentHash,
    pub to_hash: ContentHash,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RenameMatcherStats {
    pub deleted_files: usize,
    pub added_files: usize,
    pub exact_hash_matches: usize,
    pub total_possible_pairs: usize,
    pub metadata_candidate_pairs: usize,
    pub scored_pairs: usize,
    pub semantic_scored_pairs: usize,
    pub high_confidence_delta_pairs: usize,
    pub threshold_matches: usize,
    pub matched_pairs: usize,
    pub blob_loads: usize,
    pub blob_bytes_loaded: usize,
    pub used_content: bool,
}

#[derive(Debug)]
pub(crate) struct RenameDetection {
    pub matches: HashMap<String, RenameMatch>,
    pub stats: RenameMatcherStats,
}

pub(crate) type FlatTree = HashMap<String, (ContentHash, EntryType)>;

pub(crate) const DEFAULT_THRESHOLD: f64 = 0.55;

const WEIGHT_DELTA: f64 = 0.50;
const WEIGHT_SEMANTIC: f64 = 0.30;
const WEIGHT_PATH: f64 = 0.20;
const HIGH_CONFIDENCE_DELTA: f64 = 0.95;
const CANDIDATE_CAP: usize = 10_000;
const MAX_SIZE_RATIO_USIZE: usize = 3;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RenameMatcherConfig {
    pub threshold: f64,
}

impl Default for RenameMatcherConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

#[derive(Clone, Copy)]
struct CandidatePath<'a> {
    path: &'a str,
    basename: &'a str,
    stem: &'a str,
    extension: Option<&'a str>,
}

#[derive(Clone)]
struct CandidateFile<'a> {
    index: usize,
    path: CandidatePath<'a>,
    size: Option<usize>,
    content: Option<Vec<u8>>,
}

struct AddedIndex<'a> {
    files: Vec<CandidateFile<'a>>,
    all_indices: Vec<usize>,
    by_basename: HashMap<&'a str, Vec<usize>>,
    by_stem: HashMap<&'a str, Vec<usize>>,
    by_extension: HashMap<Option<&'a str>, Vec<usize>>,
    sorted_by_size: Vec<usize>,
}

pub(crate) fn flatten_tree(
    store: &impl BlockingObjectStore,
    tree: &Tree,
    prefix: &str,
) -> Result<FlatTree> {
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

pub(crate) fn detect_renames(
    store: &impl BlockingObjectStore,
    base: &FlatTree,
    branch: &FlatTree,
    config: RenameMatcherConfig,
) -> Result<HashMap<String, RenameMatch>> {
    Ok(detect_renames_with_stats(store, base, branch, config)?.matches)
}

pub(crate) fn detect_renames_with_stats(
    store: &impl BlockingObjectStore,
    base: &FlatTree,
    branch: &FlatTree,
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
                        &mut stats,
                    ),
                    _ => path_score * WEIGHT_PATH,
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

fn load_candidate_files<'a>(
    store: &impl BlockingObjectStore,
    entries: &[(usize, &'a str, &ContentHash)],
    load_content: bool,
    stats: &mut RenameMatcherStats,
) -> Result<Vec<CandidateFile<'a>>> {
    let mut files = Vec::with_capacity(entries.len());
    for &(index, path, hash) in entries {
        let blob = store.get_blob(hash)?;
        let (size, content) = match blob {
            Some(blob) => {
                stats.blob_loads += 1;
                stats.blob_bytes_loaded += blob.content().len();
                let size = Some(blob.content().len());
                let content = load_content.then(|| blob.content().to_vec());
                (size, content)
            }
            None => (None, None),
        };

        files.push(CandidateFile {
            index,
            path: CandidatePath::from_path(path),
            size,
            content,
        });
    }
    Ok(files)
}

fn build_added_index<'a>(
    store: &impl BlockingObjectStore,
    entries: &[(usize, &'a str, &ContentHash)],
    load_content: bool,
    stats: &mut RenameMatcherStats,
) -> Result<AddedIndex<'a>> {
    let files = load_candidate_files(store, entries, load_content, stats)?;
    let mut all_indices = Vec::with_capacity(files.len());
    let mut by_basename = HashMap::new();
    let mut by_stem = HashMap::new();
    let mut by_extension = HashMap::new();

    for (position, file) in files.iter().enumerate() {
        all_indices.push(position);
        by_basename
            .entry(file.path.basename)
            .or_insert_with(Vec::new)
            .push(position);
        by_stem
            .entry(file.path.stem)
            .or_insert_with(Vec::new)
            .push(position);
        by_extension
            .entry(file.path.extension)
            .or_insert_with(Vec::new)
            .push(position);
    }

    let mut sorted_by_size = all_indices.clone();
    sorted_by_size.sort_by_key(|index| files[*index].size.unwrap_or(usize::MAX));

    for indices in by_extension.values_mut() {
        indices.sort_by_key(|index| files[*index].size.unwrap_or(usize::MAX));
    }

    Ok(AddedIndex {
        files,
        all_indices,
        by_basename,
        by_stem,
        by_extension,
        sorted_by_size,
    })
}

fn collect_candidate_positions(
    deleted_file: &CandidateFile<'_>,
    added_index: &AddedIndex<'_>,
) -> Vec<usize> {
    let mut positions = Vec::new();

    push_unique_matches(
        &mut positions,
        added_index.by_basename.get(deleted_file.path.basename),
    );
    push_unique_matches(
        &mut positions,
        added_index.by_stem.get(deleted_file.path.stem),
    );

    if let Some(indices) = added_index.by_extension.get(&deleted_file.path.extension) {
        push_unique_size_window(
            &mut positions,
            indices,
            &added_index.files,
            deleted_file.size,
        );
    }

    if positions.is_empty() {
        push_unique_size_window(
            &mut positions,
            &added_index.sorted_by_size,
            &added_index.files,
            deleted_file.size,
        );
    }

    if positions.is_empty() {
        positions.extend(added_index.all_indices.iter().copied());
    }

    positions
}

fn push_unique_matches(target: &mut Vec<usize>, matches: Option<&Vec<usize>>) {
    let Some(matches) = matches else {
        return;
    };
    for &index in matches {
        if !target.contains(&index) {
            target.push(index);
        }
    }
}

fn push_unique_size_window(
    target: &mut Vec<usize>,
    sorted_indices: &[usize],
    files: &[CandidateFile<'_>],
    size: Option<usize>,
) {
    let Some(size) = size else {
        return;
    };
    let lower = size.div_ceil(MAX_SIZE_RATIO_USIZE);
    let upper = size.saturating_mul(MAX_SIZE_RATIO_USIZE);
    let start =
        sorted_indices.partition_point(|index| files[*index].size.unwrap_or(usize::MAX) < lower);
    let end =
        sorted_indices.partition_point(|index| files[*index].size.unwrap_or(usize::MAX) <= upper);

    for &index in &sorted_indices[start..end] {
        if !target.contains(&index) {
            target.push(index);
        }
    }
}

impl<'a> CandidatePath<'a> {
    fn from_path(path: &'a str) -> Self {
        let basename = file_name_of(path);
        Self {
            path,
            basename,
            stem: file_stem_of(basename),
            extension: extension_of(path),
        }
    }
}

fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn file_stem_of(path: &str) -> &str {
    path.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(path)
}

fn composite_score(
    from_path: &str,
    to_path: &str,
    from_content: &[u8],
    to_content: &[u8],
    path_score: f64,
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
        compute_semantic_similarity(from_path, to_path, from_content, to_content)
    };

    // When the semantic component contributes nothing — either the
    // build doesn't carry the `semantic` feature or the language is
    // unsupported — redistribute its weight onto delta. Without this
    // a small modified rename (e.g. ~10% byte change in a 40-byte
    // file) scores 0.5 * delta_score ≈ 0.31 against a 0.55 threshold
    // and silently misses, even though the delta similarity itself
    // is unambiguously high. Folding the orphaned semantic weight
    // into delta keeps the threshold semantics correct without
    // moving the constants for the common (semantic-on) build.
    if semantic_score == 0.0 {
        let delta_weight = WEIGHT_DELTA + WEIGHT_SEMANTIC;
        return delta_weight * delta_score + WEIGHT_PATH * path_score;
    }

    WEIGHT_DELTA * delta_score + WEIGHT_SEMANTIC * semantic_score + WEIGHT_PATH * path_score
}

pub(crate) fn delta_similarity(base: &[u8], target: &[u8]) -> f64 {
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

fn compute_semantic_similarity(
    from_path: &str,
    to_path: &str,
    from_content: &[u8],
    to_content: &[u8],
) -> f64 {
    #[cfg(not(feature = "semantic"))]
    {
        let _ = (from_path, to_path, from_content, to_content);
        0.0
    }
    #[cfg(feature = "semantic")]
    {
        let Ok(from_str) = std::str::from_utf8(from_content) else {
            return 0.0;
        };
        let Ok(to_str) = std::str::from_utf8(to_content) else {
            return 0.0;
        };

        let language = semantic::parser::Language::from_path(std::path::Path::new(from_path));
        let language = if language == semantic::parser::Language::Unknown {
            semantic::parser::Language::from_path(std::path::Path::new(to_path))
        } else {
            language
        };

        semantic::analysis::analysis_similarity::compute_similarity_with_language(
            from_str,
            to_str,
            semantic::analysis::analysis_similarity::SimilarityMethod::Ast,
            language,
        )
    }
}

pub(crate) fn path_similarity(left: &str, right: &str) -> f64 {
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

pub(crate) fn infer_directory_renames(
    renames: &HashMap<String, RenameMatch>,
) -> Vec<(String, String)> {
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

fn extension_of(path: &str) -> Option<&str> {
    file_name_of(path)
        .rsplit_once('.')
        .map(|(_, extension)| extension)
}

fn parent_dir(path: &str) -> Option<String> {
    path.rsplit_once('/')
        .map(|(directory, _)| directory.to_string())
}
