// SPDX-License-Identifier: Apache-2.0
use std::{collections::HashMap, path::Path};

use objects::{
    object::{
        Blob, ContentHash, FileProvenance, LineSpan, Origin, ProvenanceError, State, Tree,
        TreeEntry,
    },
    store::ObjectStore,
};

use super::{HeddleError, Repository, Result, builder::ProvenanceBuilder};

pub(super) fn split_text_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let content = std::str::from_utf8(bytes).ok()?;
    Some(content.lines().map(str::to_string).collect())
}

pub(super) fn build_single_origin_provenance(
    file_blob: ContentHash,
    lines: &[String],
    origin: Origin,
) -> FileProvenance {
    let mut builder = ProvenanceBuilder::default();
    let origin_set = builder.origin_set_from_origins([origin]);
    builder.into_file_provenance(file_blob, lines.len(), vec![origin_set; lines.len()])
}

pub(super) fn synthesize_file_provenance_from_blob(
    blob: Option<&Blob>,
    state: &State,
) -> Option<FileProvenance> {
    let blob = blob?;
    let lines = split_text_lines(blob.content())?;
    Some(build_single_origin_provenance(
        blob.hash(),
        &lines,
        Origin {
            state_id: state.change_id,
            attribution: state.attribution.clone(),
            created_at: state.created_at,
            authored_at: state.authored_at,
        },
    ))
}

pub(super) fn load_lines_for_hash(repo: &Repository, hash: ContentHash) -> Result<Vec<String>> {
    let blob = repo
        .store()
        .get_blob(&hash)?
        .ok_or_else(|| HeddleError::NotFound(format!("blob {}", hash)))?;
    split_text_lines(blob.content())
        .ok_or_else(|| HeddleError::InvalidObject("provenance references binary data".to_string()))
}

pub(super) fn expand_line_origin_sets_with_builder(
    provenance: &FileProvenance,
    builder: &mut ProvenanceBuilder,
) -> Result<Vec<u32>> {
    let mut mapping = HashMap::new();
    let mut translated_sets = Vec::new();

    for (index, origin_set) in provenance.origin_sets.iter().enumerate() {
        let translated_indexes = origin_set
            .origin_indexes
            .iter()
            .map(|origin_index| provenance.origins[*origin_index as usize].clone())
            .map(|origin| builder.origin_index(origin))
            .collect();
        let translated = builder.origin_set_from_indexes(translated_indexes);
        mapping.insert(index as u32, translated);
    }

    for set_index in provenance
        .line_origin_set_indexes()
        .map_err(|error: ProvenanceError| HeddleError::InvalidObject(error.to_string()))?
    {
        translated_sets.push(*mapping.get(&set_index).unwrap_or(&set_index));
    }

    Ok(translated_sets)
}

pub(super) fn lcs_line_matches(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize)> {
    let n = old_lines.len();
    let m = new_lines.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut matches = Vec::new();
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches
}

pub(super) fn coalesce_line_spans(line_origin_sets: &[u32]) -> Vec<LineSpan> {
    if line_origin_sets.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0u32;
    let mut current = line_origin_sets[0];
    let mut len = 0u32;
    for &origin_set in line_origin_sets {
        if origin_set == current {
            len += 1;
        } else {
            spans.push(LineSpan {
                start_line: start,
                line_len: len,
                origin_set_index: current,
            });
            start += len;
            current = origin_set;
            len = 1;
        }
    }
    spans.push(LineSpan {
        start_line: start,
        line_len: len,
        origin_set_index: current,
    });
    spans
}

pub(super) fn lookup_tree_entry(repo: &Repository, tree: &Tree, path: &Path) -> Option<TreeEntry> {
    let (name, rest) = split_path(path)?;
    let entry = tree.get(name)?.clone();
    if rest.as_os_str().is_empty() {
        return Some(entry);
    }
    if !entry.is_tree() {
        return None;
    }
    let subtree = repo.store().get_tree(&entry.hash).ok().flatten()?;
    lookup_tree_entry(repo, &subtree, rest)
}

pub(super) fn split_path(path: &Path) -> Option<(&str, &Path)> {
    let mut components = path.components();
    let first = components.next()?;
    let std::path::Component::Normal(name) = first else {
        return None;
    };
    Some((name.to_str()?, components.as_path()))
}
