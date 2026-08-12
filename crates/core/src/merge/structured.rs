// SPDX-License-Identifier: Apache-2.0
//! Projection from merge-engine hunks into the durable conflict object model.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result};
use merge::{ConflictLineRange, TextConflictRegion, text_conflict_regions};
use objects::object::{
    ConflictRange, ConflictRegion, ConflictSide, ContentHash, StateId, StructuredConflict, Tree,
};
use repo::Repository;

pub fn build_conflict_payload(
    repo: &Repository,
    base: (StateId, &Tree),
    ours: (StateId, &Tree),
    theirs: (StateId, &Tree),
    merged_tree: &Tree,
    conflict_paths: &[String],
) -> Result<StructuredConflict> {
    let mut conflicts = Vec::new();
    let mut occurrences = HashMap::new();
    for path in conflict_paths {
        let Some(merged) = blob_at_path(repo, merged_tree, path)? else {
            continue;
        };
        let base_blob = blob_at_path(repo, base.1, path)?.unwrap_or_default();
        let our_blob = blob_at_path(repo, ours.1, path)?.unwrap_or_default();
        let their_blob = blob_at_path(repo, theirs.1, path)?.unwrap_or_default();
        let marker_ranges = marker_ranges(&merged.bytes);
        if marker_ranges.is_empty() {
            continue;
        }
        let analyzed = text_conflict_regions(&base_blob.bytes, &our_blob.bytes, &their_blob.bytes);
        let regions = align_regions(analyzed, &marker_ranges, &base_blob, &our_blob, &their_blob);
        for region in regions {
            let symbol = conflict_symbol(path, &our_blob.bytes, region.ours)
                .or_else(|| conflict_symbol(path, &their_blob.bytes, region.theirs))
                .or_else(|| conflict_symbol(path, &base_blob.bytes, region.base));
            let base_side = conflict_side(base.0, &base_blob, region.base)?;
            let our_side = conflict_side(ours.0, &our_blob, region.ours)?;
            let their_side = conflict_side(theirs.0, &their_blob, region.theirs)?;
            let occurrence_key = (
                path.clone(),
                symbol.clone(),
                base_side.hunk_hash,
                our_side.hunk_hash,
                their_side.hunk_hash,
            );
            let occurrence = occurrences.entry(occurrence_key).or_insert(0u32);
            conflicts.push(
                ConflictRegion::new(
                    path,
                    symbol,
                    *occurrence,
                    conflict_range(region.merged)?,
                    base_side,
                    our_side,
                    their_side,
                )
                .context("construct structured conflict region")?,
            );
            *occurrence += 1;
        }
    }
    let payload = StructuredConflict::new(conflicts);
    payload.validate()?;
    Ok(payload)
}

#[derive(Default)]
struct BlobAtPath {
    id: Option<ContentHash>,
    bytes: Vec<u8>,
}

fn blob_at_path(repo: &Repository, root: &Tree, path: &str) -> Result<Option<BlobAtPath>> {
    let mut tree = root.clone();
    let mut components = Path::new(path).components().peekable();
    while let Some(component) = components.next() {
        let Some(name) = component.as_os_str().to_str() else {
            return Ok(None);
        };
        let Some(entry) = tree.get(name) else {
            return Ok(None);
        };
        if components.peek().is_some() {
            let Some(tree_hash) = entry.tree_hash() else {
                return Ok(None);
            };
            tree = repo.require_tree(&tree_hash)?;
            continue;
        }
        let Some(blob_id) = entry.leaf_content_hash() else {
            return Ok(None);
        };
        let blob = repo.require_blob(&blob_id)?;
        return Ok(Some(BlobAtPath {
            id: Some(blob_id),
            bytes: blob.content().to_vec(),
        }));
    }
    Ok(None)
}

fn align_regions(
    analyzed: Vec<TextConflictRegion>,
    marker_ranges: &[ConflictLineRange],
    base: &BlobAtPath,
    ours: &BlobAtPath,
    theirs: &BlobAtPath,
) -> Vec<TextConflictRegion> {
    if analyzed.len() == marker_ranges.len() {
        return analyzed
            .into_iter()
            .zip(marker_ranges)
            .map(|(mut region, marker)| {
                region.merged = *marker;
                region
            })
            .collect();
    }
    marker_ranges
        .iter()
        .map(|merged| TextConflictRegion {
            base: whole_range(&base.bytes),
            ours: whole_range(&ours.bytes),
            theirs: whole_range(&theirs.bytes),
            merged: *merged,
        })
        .collect()
}

fn marker_ranges(bytes: &[u8]) -> Vec<ConflictLineRange> {
    let lines: Vec<&[u8]> = bytes.split_inclusive(|byte| *byte == b'\n').collect();
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with(b"<<<<<<< ") {
            start = Some(index);
        } else if line.starts_with(b">>>>>>> ")
            && let Some(start) = start.take()
        {
            ranges.push(ConflictLineRange {
                start,
                end: index + 1,
            });
        }
    }
    ranges
}

fn whole_range(bytes: &[u8]) -> ConflictLineRange {
    ConflictLineRange {
        start: 0,
        end: bytes.split_inclusive(|byte| *byte == b'\n').count(),
    }
}

fn conflict_side(
    state: StateId,
    blob: &BlobAtPath,
    range: ConflictLineRange,
) -> Result<ConflictSide> {
    ConflictSide::new(state, blob.id, conflict_range(range)?, &blob.bytes)
        .context("construct structured conflict side")
}

fn conflict_range(range: ConflictLineRange) -> Result<ConflictRange> {
    ConflictRange::new(range.start, range.end).context("convert structured conflict range")
}

#[cfg(feature = "semantic")]
fn conflict_symbol(path: &str, bytes: &[u8], range: ConflictLineRange) -> Option<String> {
    let start = u32::try_from(range.start).ok()?.saturating_add(1);
    let end = u32::try_from(range.end.max(range.start + 1)).ok()?;
    semantic::symbol_resolver::extract_definitions(bytes, Path::new(path))
        .ok()?
        .into_iter()
        .filter(|definition| definition.start_line <= start && definition.end_line >= end)
        .min_by_key(|definition| definition.end_line - definition.start_line)
        .map(|definition| match definition.parent_name {
            Some(parent) => format!("{parent}::{}", definition.name),
            None => definition.name,
        })
}

#[cfg(not(feature = "semantic"))]
fn conflict_symbol(_path: &str, _bytes: &[u8], _range: ConflictLineRange) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_ranges_are_zero_based_and_half_open() {
        let marked = b"before\n<<<<<<< OURS\nours\n=======\ntheirs\n>>>>>>> THEIRS\nafter\n";
        assert_eq!(
            marker_ranges(marked),
            vec![ConflictLineRange { start: 1, end: 6 }]
        );
    }
}
