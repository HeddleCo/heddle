// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use anyhow::Result;
use objects::{object::ContentHash, store::ObjectStore};

use super::{
    RenameMatcherStats,
    scoring::{extension_of, file_name_of, file_stem_of},
};

const MAX_SIZE_RATIO_USIZE: usize = 3;

#[derive(Clone, Copy)]
pub(super) struct CandidatePath<'a> {
    pub(super) path: &'a str,
    pub(super) basename: &'a str,
    pub(super) stem: &'a str,
    pub(super) extension: Option<&'a str>,
}

#[derive(Clone)]
pub(super) struct CandidateFile<'a> {
    pub(super) index: usize,
    pub(super) path: CandidatePath<'a>,
    pub(super) size: Option<usize>,
    pub(super) content: Option<Vec<u8>>,
}

pub(super) struct AddedIndex<'a> {
    pub(super) files: Vec<CandidateFile<'a>>,
    pub(super) all_indices: Vec<usize>,
    by_basename: HashMap<&'a str, Vec<usize>>,
    by_stem: HashMap<&'a str, Vec<usize>>,
    by_extension: HashMap<Option<&'a str>, Vec<usize>>,
    sorted_by_size: Vec<usize>,
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

pub(super) fn load_candidate_files<'a>(
    store: &impl ObjectStore,
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

pub(super) fn build_added_index<'a>(
    store: &impl ObjectStore,
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

pub(super) fn collect_candidate_positions(
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
