// SPDX-License-Identifier: Apache-2.0
//! Build canonical FileProvenance from a completed origin stream.

use crate::object::{ContentHash, FileProvenance, Origin, OriginSet};

use super::types::{BlameSliceError, OriginRange};

/// Assemble [`FileProvenance`] from finalized ranges without holding history.
///
/// Ranges are sorted, checked for exact coverage, and coalesced when adjacent
/// ranges share the same origin.
pub fn finalize_file_provenance(
    file_blob: ContentHash,
    line_count: u32,
    ranges: impl IntoIterator<Item = OriginRange>,
) -> Result<FileProvenance, BlameSliceError> {
    let mut ranges: Vec<OriginRange> = ranges.into_iter().collect();
    ranges.sort_by_key(|range| range.target_start);

    if line_count == 0 {
        if ranges
            .iter()
            .any(|range| range.target_start != 0 || range.len != 0)
        {
            return Err(BlameSliceError::InvalidCoverage);
        }
        let origins = ranges
            .first()
            .map(|range| vec![range.origin.clone()])
            .unwrap_or_default();
        let origin_sets = if origins.is_empty() {
            Vec::new()
        } else {
            vec![OriginSet {
                origin_indexes: vec![0],
            }]
        };
        let provenance = FileProvenance::new(file_blob, 0, Vec::new(), origins, origin_sets);
        provenance
            .validate()
            .map_err(|_| BlameSliceError::InvalidCoverage)?;
        return Ok(provenance);
    }

    let coalesced = coalesce(ranges);
    let mut next_line = 0u32;
    let mut origins: Vec<Origin> = Vec::new();
    let mut origin_sets = Vec::new();
    let mut spans = Vec::new();

    for range in coalesced {
        if range.target_start != next_line || range.len == 0 {
            return Err(BlameSliceError::InvalidCoverage);
        }
        let origin_index = origin_index(&mut origins, range.origin);
        let origin_set_index = origin_set_index(&mut origin_sets, origin_index);
        spans.push(crate::object::LineSpan {
            start_line: range.target_start,
            line_len: range.len,
            origin_set_index,
        });
        next_line = next_line.saturating_add(range.len);
    }
    if next_line != line_count {
        return Err(BlameSliceError::InvalidCoverage);
    }

    let provenance = FileProvenance::new(file_blob, line_count, spans, origins, origin_sets);
    provenance
        .validate()
        .map_err(|_| BlameSliceError::InvalidCoverage)?;
    Ok(provenance)
}

fn coalesce(ranges: Vec<OriginRange>) -> Vec<OriginRange> {
    let mut out: Vec<OriginRange> = Vec::new();
    for range in ranges {
        match out.last_mut() {
            Some(last)
                if last.target_start + last.len == range.target_start
                    && last.origin == range.origin =>
            {
                last.len += range.len;
            }
            _ => out.push(range),
        }
    }
    out
}

fn origin_index(origins: &mut Vec<Origin>, origin: Origin) -> u32 {
    if let Some((index, _)) = origins
        .iter()
        .enumerate()
        .find(|(_, existing)| **existing == origin)
    {
        return index as u32;
    }
    let next = origins.len() as u32;
    origins.push(origin);
    next
}

fn origin_set_index(origin_sets: &mut Vec<OriginSet>, origin_index: u32) -> u32 {
    let indexes = vec![origin_index];
    if let Some((index, _)) = origin_sets
        .iter()
        .enumerate()
        .find(|(_, set)| set.origin_indexes == indexes)
    {
        return index as u32;
    }
    let next = origin_sets.len() as u32;
    origin_sets.push(OriginSet {
        origin_indexes: indexes,
    });
    next
}
